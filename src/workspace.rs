use std::fs;
use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::{Deserialize, Serialize};

use crate::backend::{RunningInstance, SshTarget};
use crate::config::{Instance, Mount};
use crate::paths::GuestPath;
use crate::remote_command::RemoteCommand;

const GUEST_WORKSPACE: &str = "/workspace";

/// Default exclusions for transfers — reproducible build/cache directories
/// only. Git metadata is handled separately because the safe policy depends on
/// the transfer direction.
const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules/",
    "target/",
    "__pycache__/",
    ".venv/",
    ".coop/",
];

/// Best-effort match for Git administration entries in every ASCII case.
///
/// macOS filesystems are commonly case-insensitive, so allowing a guest `.GIT`
/// entry through a case-sensitive filter can still overwrite the host's
/// `.git`. The bracket form works with rsync and matches at every
/// directory depth. This filter reduces accidental transfer; it is not a
/// security boundary because transport and filesystem name semantics vary.
const GIT_EXCLUDE: &str = ".[gG][iI][tT]";

const PULL_UNTRUSTED_WARNING: &str = "Pull copies files controlled by the untrusted guest. \
Treat the result as potentially malicious: review it before executing files or interpreting \
it with Git, editors, build tools, shells, or other host applications. Git metadata filtering \
is best effort, not a security boundary.";

/// Persisted workspace metadata written during `start`.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Path inside the guest VM. Always absolute — enforced on
    /// deserialize so a hand-edited or pre-migration `workspace.json`
    /// with a relative path is rejected at load time rather than
    /// flowing into the remote shell commands that interpolate it.
    #[serde(deserialize_with = "deserialize_absolute_guest_path")]
    pub guest_path: GuestPath,
    /// How the workspace was created. Variant-specific fields (host
    /// path, original repo URL) live inside the source so invalid
    /// combinations can't be expressed.
    pub source: WorkspaceSource,
}

/// How the workspace contents arrived in the guest.
///
/// `Workspace` and `Mount` always have a host directory (pushed/synced
/// or live-mounted). `GitRepo` clones happen entirely inside the guest
/// — there is no host-side copy, only the original URL used to
/// re-derive the `owner/repo` slug for follow-up commands.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceSource {
    Workspace { host_path: PathBuf },
    GitRepo { url: crate::github_repo::GitRepoUrl },
    Mount { host_path: PathBuf },
}

impl WorkspaceSource {
    /// Return the host path associated with this source, if any.
    ///
    /// `GitRepo` has no host path: the clone exists only inside the
    /// guest.
    pub fn host_path(&self) -> Option<&Path> {
        match self {
            Self::Workspace { host_path } | Self::Mount { host_path } => Some(host_path),
            Self::GitRepo { .. } => None,
        }
    }
}

/// Deserialize a `guest_path`, routing through [`GuestPath::absolute`].
///
/// `GuestPath` deserializes transparently (any string), which would let a
/// relative or empty `guest_path` in a hand-edited `workspace.json` bypass
/// the absolute-path invariant the rest of the code relies on. Enforcing it
/// here keeps the invariant true for every loaded `WorkspaceState`.
fn deserialize_absolute_guest_path<'de, D>(deserializer: D) -> Result<GuestPath, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let raw = String::deserialize(deserializer)?;
    GuestPath::absolute(raw).map_err(D::Error::custom)
}

impl WorkspaceState {
    pub fn save(&self, inst: &Instance) -> Result<()> {
        let path = inst.workspace_state_path();
        let json =
            serde_json::to_string_pretty(self).context("Failed to serialize workspace state")?;
        crate::fs_util::atomic_write_json(&path, &json)
            .context("Failed to write workspace.json")?;
        tracing::debug!("Wrote workspace state to {}", path.display());
        Ok(())
    }

    pub fn try_load(inst: &Instance) -> Result<Option<Self>> {
        let path = inst.workspace_state_path();
        let content = match crate::fs_util::read_optional_private(&path) {
            Ok(Some(content)) => content,
            Ok(None) => return Ok(None),
            Err(e) => {
                return Err(e.context(format!("Failed to read {}", path.display())));
            }
        };
        serde_json::from_str(&content)
            .with_context(|| {
                format!(
                    "Failed to parse {}.\n\
                     If this file was written by a pre-#147 coop the on-disk \
                     shape changed; delete it and run `coop up` again to \
                     regenerate, or `coop destroy <name>` if the instance is \
                     no longer needed.",
                    path.display()
                )
            })
            .map(Some)
    }
}

/// Load `workspace.json`, logging a `WARN` on parse errors.
///
/// Returns `None` when no state file exists (the instance simply hasn't
/// been started, or was started by a coop too old to write one) and also
/// when the file exists but cannot be deserialized — typically a pre-#147
/// shape that the current loader rejects. The parse-error case logs a
/// warning that includes the loader's migration hint and the supplied
/// `consequence` so the user can see which feature degraded (pat-mode
/// token forwarding, workspace-affinity matching, etc.) rather than the
/// failure being silently swallowed.
pub fn try_load_or_warn(inst: &Instance, consequence: &str) -> Option<WorkspaceState> {
    match WorkspaceState::try_load(inst) {
        Ok(state) => state,
        Err(e) => {
            tracing::warn!(
                "Could not read workspace state for '{}'; {consequence}. {e:#}",
                inst.name,
            );
            None
        }
    }
}

/// Copy a local workspace to the guest without deleting existing guest files.
pub fn copy_workspace(target: &SshTarget, source_dir: &Path, exclude_git: bool) -> Result<()> {
    rsync_to_guest(
        target,
        source_dir,
        &default_workspace_path(),
        exclude_git,
        SyncMode::Copy,
    )
}

/// The coop convention for where a workspace lands inside the guest.
///
/// Use this in preference to constructing `GuestPath::absolute("/workspace")`
/// at call sites: it's the single source of truth for the default mount
/// target and never fails (the underlying constant is a static absolute path).
#[expect(
    clippy::expect_used,
    reason = "GUEST_WORKSPACE is a const literal starting with '/'; invariant holds at compile time"
)]
pub(crate) fn default_workspace_path() -> GuestPath {
    GuestPath::absolute(GUEST_WORKSPACE).expect("GUEST_WORKSPACE constant is absolute")
}

/// Which project-mount policy applies when assembling a start-time mount
/// set. Only the workspace-collision message differs between rules; every
/// rule also enforces guest-path uniqueness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceMountRule {
    /// `coop up --copy`: the project occupies `/workspace` via copy, so an
    /// extra mount targeting `/workspace` collides.
    CopyProject,
    /// `coop up --git-repo`: the clone occupies `/workspace`.
    GitRepoClone,
    /// The project is itself mounted at `/workspace` (`coop up --mount`), or
    /// there is no project mount (`coop quickstart`). `/workspace` is either
    /// legitimately in use or absent, so only uniqueness applies (a second
    /// `/workspace` mount is still caught, as a duplicate guest path).
    ProjectMountedOrNone,
}

/// A start-time mount set that has passed collection-level validation: no
/// two mounts target the same guest path, and — per [`WorkspaceMountRule`]
/// — no extra mount collides with the project's `/workspace`.
///
/// [`Self::assemble`] is the single place these invariants are enforced, so
/// no lifecycle path (`up --copy`, `up --mount`, `up --git-repo`,
/// `quickstart`) can build a mount set and forget to check it.
#[derive(Debug)]
pub(crate) struct ValidatedMounts(Vec<Mount>);

impl ValidatedMounts {
    /// Validate an assembled mount set under `rule`. `mounts` is the final
    /// set (project mount already inserted for `--mount`, extra mounts
    /// appended), so the checks see exactly what boot will use.
    pub(crate) fn assemble(rule: WorkspaceMountRule, mounts: Vec<Mount>) -> Result<Self> {
        let collision_message = match rule {
            WorkspaceMountRule::CopyProject => Some(
                "`coop up --copy` already uses /workspace for the project. \
                 Give --extra-mount an explicit non-/workspace guest path, or use \
                 `coop up --mount` to mount the project itself.",
            ),
            WorkspaceMountRule::GitRepoClone => Some(
                "`coop up --git-repo` clones into /workspace. \
                 Give --extra-mount an explicit non-/workspace guest path.",
            ),
            WorkspaceMountRule::ProjectMountedOrNone => None,
        };
        if let Some(message) = collision_message {
            let workspace_path = default_workspace_path().to_string();
            if mounts
                .iter()
                .any(|mount| mount.guest_path.to_string() == workspace_path)
            {
                bail!("{message}");
            }
        }
        crate::config::validate_unique_guest_paths(&mounts)?;
        Ok(Self(mounts))
    }

    /// The validated mount set, ready to hand to [`crate::commands::StartOpts`].
    pub(crate) fn into_vec(self) -> Vec<Mount> {
        self.0
    }
}

fn load_or_default(inst: &Instance, dir: Option<&str>, cmd: &str) -> Result<WorkspaceState> {
    if let Some(state) = WorkspaceState::try_load(inst)? {
        return Ok(state);
    }
    if let Some(d) = dir {
        return Ok(WorkspaceState {
            guest_path: default_workspace_path(),
            source: WorkspaceSource::Workspace {
                host_path: PathBuf::from(d),
            },
        });
    }
    bail!(
        "No workspace.json found and no --dir given.\n\
         Either create/reconnect with `coop up <PATH>` or provide a path: \
         coop {cmd} --dir ./my-project"
    )
}

/// Push a local directory to the guest via rsync.
///
/// Takes a `RunningInstance` so the caller's recent live-state observation is
/// visible in the signature. The guest can still stop before SSH connects.
pub fn push(
    running: &RunningInstance,
    dir: Option<&str>,
    force: bool,
    exclude_git: bool,
) -> Result<()> {
    let inst = running.instance();
    let target = running.target();
    let state = load_or_default(inst, dir, "push")?;
    let source_dir = resolve_host_dir(dir, &state, "push")?;

    if !source_dir.is_dir() {
        bail!("Source directory {} does not exist", source_dir.display());
    }

    if !force {
        check_guest_dirty(target, &state.guest_path)?;
    }

    tracing::info!(
        "Pushing {} -> guest:{}",
        source_dir.display(),
        state.guest_path
    );

    rsync_push(target, &source_dir, &state.guest_path, exclude_git)?;

    tracing::info!("Push complete");
    Ok(())
}

/// Pull the guest workspace to a local directory via rsync.
///
/// Takes a `RunningInstance` so the caller's recent live-state observation is
/// visible in the signature. The guest can still stop before SSH connects.
pub fn pull(running: &RunningInstance, dir: Option<&str>, force: bool) -> Result<()> {
    let inst = running.instance();
    let target = running.target();
    let state = load_or_default(inst, dir, "pull")?;
    let dest_dir = resolve_host_dir(dir, &state, "pull")?;
    let operation = PullOperation::new(pull_staging_path(inst)?)?;

    if !force && pull_destination_is_nonempty(&dest_dir)? {
        bail!(
            "Refusing to pull into non-empty destination '{}'.\n\
             coop does not inspect the destination with Git because its files and repository \
             metadata may be untrusted. Review the destination, then rerun with --force to \
             authorize overwriting matching files. --force does not make pulled content trusted.",
            dest_dir.display()
        );
    }

    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("Failed to create {}", dest_dir.display()))?;

    tracing::warn!("{PULL_UNTRUSTED_WARNING}");

    tracing::info!(
        "Pulling guest:{} -> {}",
        state.guest_path,
        dest_dir.display()
    );

    rsync_pull(target, &state.guest_path, &dest_dir, &operation)?;

    tracing::info!("Pull complete");
    Ok(())
}

/// Sync mount directories to the guest via rsync (Firecracker only).
///
/// Creates guest directories, transfers files, and saves workspace
/// state so `push`/`pull` work for ongoing sync.
pub fn sync_mounts(
    target: &SshTarget,
    inst: &Instance,
    mounts: &[crate::config::Mount],
    exclude_git: bool,
) -> Result<()> {
    sync_mount_contents(target, mounts, exclude_git)?;
    record_mount_state(inst, mounts)
}

/// Sync mount directories to the guest without changing workspace metadata.
pub fn sync_mount_contents(
    target: &SshTarget,
    mounts: &[crate::config::Mount],
    exclude_git: bool,
) -> Result<()> {
    for m in mounts {
        let guest = &m.guest_path;
        target.exec(
            RemoteCommand::new()
                .literal("sudo mkdir -p ")
                .arg(guest)
                .literal(" && sudo chown ubuntu:ubuntu ")
                .arg(guest),
        )?;

        tracing::info!("Syncing {} -> guest:{guest}", m.host_path.display(),);

        rsync_push(target, &m.host_path, guest, exclude_git)?;
    }
    Ok(())
}

/// Persist mount metadata to `workspace.json`.
///
/// Records the first mount's host path as the canonical workspace so
/// that `push`/`pull` and PAT slug detection (`detect_instance_repo`)
/// work on follow-up commands. Used by `sync_mounts` after a Firecracker
/// rsync, and directly on Lima where mounts are live (no sync step).
pub fn record_mount_state(inst: &Instance, mounts: &[crate::config::Mount]) -> Result<()> {
    if let Some(m) = mounts.first() {
        let state = WorkspaceState {
            guest_path: m.guest_path.clone(),
            source: WorkspaceSource::Mount {
                host_path: m.host_path.clone(),
            },
        };
        state.save(inst)?;
    }
    Ok(())
}

/// Editor that can attach to the guest over SSH remote development.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EditorKind {
    /// VS Code, via the Remote-SSH extension
    Code,
    /// Zed, via `ssh://` remoting
    Zed,
}

/// Generate SSH config and launch an editor connected over SSH.
///
/// Takes a `RunningInstance` so the caller's recent live-state observation is
/// visible in the signature — the editor needs an SSH target.
pub fn open_editor(
    running: &RunningInstance,
    project: Option<&str>,
    editor: Option<EditorKind>,
) -> Result<()> {
    let inst = running.instance();
    let remote_path = match project {
        Some(p) => GuestPath::absolute(p)
            .with_context(|| format!("--project must be an absolute guest path: {p:?}"))?,
        None => default_workspace_path(),
    };

    write_ssh_config(running)?;
    tracing::warn!(
        "Connecting a host editor to the VM may allow a compromised guest to execute commands \
on your host. Only connect to guests you trust."
    );
    launch_editor(inst, &remote_path, editor)?;
    Ok(())
}

/// Install (or refresh) the `coop-<name>` SSH alias and print it.
///
/// Writes the `Host coop-<name>` block to `~/.ssh/config` idempotently,
/// then prints the block plus `ssh`/`scp`/`rsync` usage hints to stderr.
/// Shared by `coop editor` (which also launches an editor) and the
/// standalone `coop ssh-config` command.
///
/// Takes a `RunningInstance` so the SSH target is proven live by the
/// type — a stale alias is worse than none.
pub fn write_ssh_config(running: &RunningInstance) -> Result<()> {
    let inst = running.instance();
    let target = running.target();

    update_ssh_config(target, inst)?;

    let host = ssh_config_host(inst);
    let block = ssh_config_block(target, inst)?;
    writeln!(
        std::io::stderr(),
        "\nSSH alias '{host}' is ready:\n\n{block}\n\n\
         Use it with ssh/scp/rsync, e.g.:\n\
         \x20   ssh {host}\n\
         \x20   scp ./file {host}:/workspace/\n\
         \x20   rsync -az ./dir/ {host}:/workspace/dir/\n\n\
         These connections skip host-key verification \
         (the VM's keys are ephemeral)."
    )
    .context("Failed to write SSH config info")?;

    Ok(())
}

/// Refresh an already-installed `coop-<name>` alias after a restart.
///
/// Only rewrites the block if one already exists for this instance, so a
/// restart never installs SSH config for a user who never ran
/// `coop ssh-config`. Keeps a Lima alias current across stop/start, where
/// the forwarded port changes (Firecracker ports are stable, so this is a
/// no-op rewrite there).
pub fn refresh_ssh_config_if_present(target: &SshTarget, inst: &Instance) -> Result<()> {
    let ssh_config = ssh_config_path()?;
    if !ssh_config.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&ssh_config).context("Failed to read ~/.ssh/config")?;
    if !marker_block_present(&content, &ssh_config_host(inst)) {
        return Ok(());
    }

    update_ssh_config(target, inst)
}

/// Whether `~/.ssh/config` content already holds a coop block for `host`.
fn marker_block_present(content: &str, host: &str) -> bool {
    let marker = format!("{MARKER_PREFIX} {host}");
    content.lines().any(|line| line.trim() == marker)
}

/// Remove SSH config blocks for all instances from ~/.ssh/config.
pub fn remove_all_ssh_config() -> Result<()> {
    remove_all_ssh_config_at(&ssh_config_path()?)
}

/// Strip every coop marker block from `ssh_config`, rewriting it only when
/// something was removed. A missing file is a no-op. Split from the public
/// entry point so the read/clean/gated-write logic is testable against a
/// temp path without mutating the process's `$HOME`.
fn remove_all_ssh_config_at(ssh_config: &Path) -> Result<()> {
    if !ssh_config.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(ssh_config).context("Failed to read ~/.ssh/config")?;

    let cleaned = remove_marker_blocks(&content);
    if cleaned.len() != content.len() {
        atomic_write(ssh_config, &cleaned).context("Failed to write ~/.ssh/config")?;
        tracing::info!("Removed coop SSH config blocks");
    }

    Ok(())
}

/// Remove SSH config block for a specific instance.
pub fn remove_ssh_config(inst: &Instance) -> Result<()> {
    remove_ssh_config_at(&ssh_config_path()?, inst)
}

/// Strip the coop marker block for `inst` from `ssh_config`, rewriting it
/// only when that block was present. A missing file is a no-op. Split from
/// the public entry point for the same reason as
/// [`remove_all_ssh_config_at`].
fn remove_ssh_config_at(ssh_config: &Path, inst: &Instance) -> Result<()> {
    if !ssh_config.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(ssh_config).context("Failed to read ~/.ssh/config")?;
    let host_name = ssh_config_host(inst);

    let cleaned = remove_named_marker_block(&content, &host_name);
    if cleaned.len() != content.len() {
        atomic_write(ssh_config, &cleaned).context("Failed to write ~/.ssh/config")?;
        tracing::info!("Removed SSH config block for instance '{}'", inst.name);
    }

    Ok(())
}

// ── Transport: rsync ──────────────────────────────────────────

fn rsync_base_args(target: &SshTarget, exclude_git: bool) -> Result<Vec<String>> {
    let mut args = vec!["-az".to_string(), "-e".to_string(), target.rsync_ssh_cmd()?];
    // `.git/` rule must precede the per-directory `.gitignore` merge: rsync
    // uses first-match-wins, so without this a repo whose `.gitignore`
    // happens to list `.git/` would silently strip git state from the
    // transfer regardless of `--exclude-git`.
    if exclude_git {
        args.push(format!("--exclude={GIT_EXCLUDE}"));
    } else {
        // `/.git/***` matches the directory itself and everything inside.
        args.push("--filter=+ /.git/***".to_string());
    }
    args.push("--filter=:- .gitignore".to_string());
    for exc in DEFAULT_EXCLUDES {
        args.push(format!("--exclude={exc}"));
    }
    Ok(args)
}

pub(crate) fn rsync_push(
    target: &SshTarget,
    source: &Path,
    guest_path: &GuestPath,
    exclude_git: bool,
) -> Result<()> {
    rsync_to_guest(target, source, guest_path, exclude_git, SyncMode::Mirror)
}

#[derive(Clone, Copy)]
enum SyncMode {
    Copy,
    Mirror,
}

fn require_guest_rsync(target: &SshTarget) -> Result<()> {
    target.exec(RemoteCommand::new().literal(
        "command -v rsync >/dev/null || { printf '%s\\n' \
         'Workspace transfers require rsync on the guest. Install rsync and retry.' >&2; exit 127; }",
    )).context("Cannot transfer workspace: guest rsync check failed")
}

fn rsync_to_guest(
    target: &SshTarget,
    source: &Path,
    guest_path: &GuestPath,
    exclude_git: bool,
    mode: SyncMode,
) -> Result<()> {
    let mut command = target
        .rsync_command()
        .context("Workspace transfers require rsync on the host")?;
    require_guest_rsync(target)?;
    let mut args = rsync_base_args(target, exclude_git)?;
    if matches!(mode, SyncMode::Mirror) {
        args.push("--delete".to_string());
    }
    args.push(format!("{}/", source.display()));
    args.push(format!("{}:{guest_path}/", target.addr()));

    let status = command
        .args(&args)
        .status()
        .context("Failed to run rsync")?;

    if !status.success() {
        bail!("rsync push failed");
    }
    Ok(())
}

fn rsync_pull(
    target: &SshTarget,
    guest_path: &GuestPath,
    dest: &Path,
    operation: &PullOperation,
) -> Result<()> {
    require_guest_rsync(target)?;
    let mut args = rsync_pull_args(target)?;
    args.push(format!("{}:{guest_path}/", target.addr()));
    // A compromised guest controls the remote rsync sender. Stage its file
    // list away from the destination, then apply the Git-metadata exclusion
    // again with the trusted host rsync during installation.
    let mut staging = operation.staging()?;
    args.push(format!("{}/", staging.path()?.display()));

    let result = (|| {
        let mut command = locked_rsync_command(target, &args, operation)?;
        let status = command.status().context("Failed to run rsync")?;

        if !status.success() {
            bail!("rsync pull failed");
        }
        install_staged_pull(target, staging.path()?, dest, operation)
    })();
    staging.finish(result)
}

fn rsync_pull_args(target: &SshTarget) -> Result<Vec<String>> {
    // Guest-authored Git administration data is active configuration, not
    // workspace content. Apply the common-name filter as defense-in-depth.
    rsync_base_args(target, true)
}

fn locked_rsync_command(
    target: &SshTarget,
    args: &[String],
    operation: &PullOperation,
) -> Result<Command> {
    let mut command = target.rsync_command()?;
    command.args(args);
    operation.inherit_lock_in(&mut command);
    Ok(command)
}

fn install_staged_pull(
    target: &SshTarget,
    source: &Path,
    dest: &Path,
    operation: &PullOperation,
) -> Result<()> {
    let args = install_staged_pull_args(source, dest);
    let mut command = locked_rsync_command(target, &args, operation)?;
    let status = command.status().context("Failed to install staged pull")?;

    if !status.success() {
        bail!("Failed to install staged pull");
    }
    Ok(())
}

fn install_staged_pull_args(source: &Path, dest: &Path) -> Vec<String> {
    vec![
        "-a".to_string(),
        format!("--exclude={GIT_EXCLUDE}"),
        format!("{}/", source.display()),
        format!("{}/", dest.display()),
    ]
}

// ── Helpers ───────────────────────────────────────────────────

fn pull_staging_path(inst: &Instance) -> Result<PathBuf> {
    let instances_dir = inst
        .dir
        .parent()
        .context("Instance directory has no parent for pull staging")?;
    let data_dir = instances_dir
        .parent()
        .context("Instances directory has no parent for pull staging")?;
    Ok(data_dir.join("pull-staging/work"))
}

/// Host-owned staging directory for data received from an untrusted guest.
///
/// The stable outer directory is private and never exposed to the transfer
/// tools. They receive into its `contents` child, whose mode the guest may
/// replace. A global lock serializes pulls sharing the coop data directory. If the process
/// dies before cleanup, the next pull removes the same bounded staging tree
/// before recreating it. Cleanup restores owner traversal permissions without
/// following symlinks. `Drop` is only a panic fallback; normal paths report a
/// cleanup failure to the caller.
struct PullOperation {
    staging_root: PathBuf,
    lock: crate::fs_util::FileLock,
}

impl PullOperation {
    fn new(staging_root: PathBuf) -> Result<Self> {
        let parent = staging_root
            .parent()
            .context("Pull staging path has no parent")?;
        let _parent = crate::fs_util::PrivateDir::create(parent)
            .context("Failed to prepare private pull staging storage")?;
        // Serialize every pull that shares this private coop data directory,
        // not just pulls from one instance. Otherwise two instances could
        // both authorize the same empty destination before either installs.
        let lock = crate::fs_util::lock_sibling_bounded(
            &parent.join("pull"),
            std::time::Duration::from_secs(30),
        )
        .context("Failed to lock pull staging directory")?;
        Ok(Self { staging_root, lock })
    }

    fn staging(&self) -> Result<PullStaging> {
        PullStaging::new(&self.staging_root)
    }

    fn inherit_lock_in(&self, command: &mut Command) {
        self.lock.inherit_in(command);
    }
}

struct PullStaging {
    root: Option<PathBuf>,
    contents: PathBuf,
}

impl PullStaging {
    fn new(root: &Path) -> Result<Self> {
        remove_staging_directory(root).context("Failed to remove stale pull staging directory")?;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(root).with_context(|| {
            format!("Failed to create pull staging directory {}", root.display())
        })?;

        let contents = root.join("contents");
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        if let Err(error) = builder.create(&contents) {
            let cleanup = fs::remove_dir(root);
            return match cleanup {
                Ok(()) => Err(error).with_context(|| {
                    format!(
                        "Failed to create pull staging contents directory {}",
                        contents.display()
                    )
                }),
                Err(cleanup_error) => Err(anyhow::Error::new(error).context(format!(
                    "Failed to create pull staging contents directory {}; additionally failed to remove {}: {cleanup_error}",
                    contents.display(),
                    root.display()
                ))),
            };
        }

        Ok(Self {
            root: Some(root.to_path_buf()),
            contents,
        })
    }

    fn path(&self) -> Result<&Path> {
        self.root
            .as_ref()
            .map(|_| self.contents.as_path())
            .context("Pull staging directory is no longer available")
    }

    fn finish(&mut self, operation: Result<()>) -> Result<()> {
        let cleanup = self.cleanup();
        match (operation, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
            (Err(error), Err(cleanup_error)) => Err(error.context(format!(
                "Additionally failed to remove pull staging directory: {cleanup_error:#}"
            ))),
        }
    }

    fn cleanup(&mut self) -> Result<()> {
        let root = self
            .root
            .take()
            .context("Pull staging directory was already cleaned")?;
        remove_staging_directory(&root).context("Failed to remove pull staging directory")
    }
}

impl Drop for PullStaging {
    fn drop(&mut self) {
        let Some(root) = self.root.take() else {
            return;
        };
        let _ = remove_staging_directory(&root);
    }
}

fn remove_staging_directory(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect staging path {}", path.display()));
        }
    };

    if metadata.file_type().is_dir() {
        let permissions = make_staging_directories_traversable(path);
        let removal = fs::remove_dir_all(path)
            .with_context(|| format!("Failed to remove staging path {}", path.display()));
        match (permissions, removal) {
            (_, Ok(())) => Ok(()),
            (Ok(()), Err(error)) => Err(error),
            (Err(permission_error), Err(removal_error)) => Err(removal_error.context(format!(
                "Failed to restore staged directory permissions before removal: \
                 {permission_error:#}"
            ))),
        }
    } else {
        fs::remove_file(path).with_context(|| {
            format!(
                "Failed to remove non-directory staging path {}",
                path.display()
            )
        })
    }
}

fn make_staging_directories_traversable(path: &Path) -> Result<()> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let metadata = fs::symlink_metadata(&directory)
            .with_context(|| format!("Failed to inspect staged path {}", directory.display()))?;
        if !metadata.file_type().is_dir() {
            continue;
        }

        let mode = metadata.permissions().mode();
        fs::set_permissions(&directory, fs::Permissions::from_mode(mode | 0o700)).with_context(
            || {
                format!(
                    "Failed to restore owner permissions on staged directory {}",
                    directory.display()
                )
            },
        )?;
        for entry in fs::read_dir(&directory).with_context(|| {
            format!(
                "Failed to traverse staged directory {}",
                directory.display()
            )
        })? {
            let entry = entry.with_context(|| {
                format!(
                    "Failed to inspect entry in staged directory {}",
                    directory.display()
                )
            })?;
            pending.push(entry.path());
        }
    }
    Ok(())
}

fn resolve_host_dir(explicit: Option<&str>, state: &WorkspaceState, cmd: &str) -> Result<PathBuf> {
    let path = if let Some(d) = explicit {
        PathBuf::from(d)
    } else {
        state
            .source
            .host_path()
            .map(Path::to_path_buf)
            .with_context(|| {
                format!(
                    "No host_path in workspace.json and no --dir given.\n\
                     Provide a directory: coop {cmd} --dir ./my-project"
                )
            })?
    };
    std::path::absolute(&path)
        .with_context(|| format!("Failed to make workspace path {} absolute", path.display()))
}

fn check_guest_dirty(target: &SshTarget, guest_path: &GuestPath) -> Result<()> {
    // Untracked files are excluded — they're almost always host-side noise
    // (build artifacts, editor state) that was copied in at start time, not
    // edits made inside the guest. Modified tracked files and unpushed
    // commits are the real signal that an agent has done work the host
    // doesn't yet know about.
    let check_cmd = RemoteCommand::new()
        .literal("if [ -e ")
        .arg(guest_path)
        .literal("/.git ]; then cd ")
        .arg(guest_path)
        .literal(
            " || exit 20; \
             git status --porcelain --untracked-files=no || exit 21; \
             if git rev-parse --abbrev-ref '@{u}' >/dev/null 2>&1; then \
                 ahead=$(git rev-list --count '@{u}..HEAD' 2>/dev/null) || exit 22; \
                 if [ \"${ahead:-0}\" -gt 0 ]; then \
                     echo \"AHEAD $ahead\"; \
                 fi; \
             fi; \
          fi",
        );

    let output = target
        .ssh_command()?
        .arg(target.addr())
        .arg(check_cmd.into_string())
        .output()
        .context("Failed to check guest workspace status")?;

    if !output.status.success() {
        let reason = match output.status.code() {
            Some(20) => "guest workspace directory could not be opened",
            Some(21) => "guest git status failed",
            Some(22) => "guest git rev-list failed",
            _ => "SSH or guest workspace status probe failed",
        };
        bail!(
            "Cannot determine guest workspace cleanliness: {reason}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        bail!(
            "Guest workspace has changes the host does not know about:\n{stdout}\n\
             Pull them into a missing or empty review directory with \
             `coop pull --dir <new-directory>`, or overwrite with `coop push --force`."
        );
    }

    Ok(())
}

fn pull_destination_is_nonempty(dest: &Path) -> Result<bool> {
    let mut entries = match fs::read_dir(dest) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect pull destination {}", dest.display()));
        }
    };

    match entries.next() {
        None => Ok(false),
        Some(Ok(_)) => Ok(true),
        Some(Err(error)) => Err(error)
            .with_context(|| format!("Failed to inspect pull destination {}", dest.display())),
    }
}

// ── SSH config / VS Code ──────────────────────────────────────

/// Write content to a file atomically via temp file + rename.
///
/// Delegates to `fs_util::atomic_write_ssh` which preserves
/// permissions from the original file (defaults to 0o600).
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    crate::fs_util::atomic_write_ssh(path, content)
}

const MARKER_PREFIX: &str = "# coop START";
const MARKER_END: &str = "# coop END";

fn ssh_config_host(inst: &Instance) -> String {
    format!("coop-{}", inst.name)
}

#[mutants::skip] // equivalent: constant getter ($HOME/.ssh/config); no caller asserts the returned PathBuf
fn ssh_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".ssh/config"))
}

fn ssh_config_block(target: &SshTarget, inst: &Instance) -> Result<String> {
    let host = ssh_config_host(inst);
    let transport = target.ssh_config_transport()?;
    Ok(format!(
        "{MARKER_PREFIX} {host}\n\
         Host {host}\n\
         {transport}\
         \x20   User {}\n\
         \x20   IdentityFile {}\n\
         \x20   IdentitiesOnly yes\n\
         \x20   IdentityAgent none\n\
         \x20   ForwardAgent no\n\
         \x20   StrictHostKeyChecking no\n\
         \x20   UserKnownHostsFile /dev/null\n\
         \x20   LogLevel ERROR\n\
         {MARKER_END}",
        target.user(),
        target.key_path().display(),
    ))
}

fn update_ssh_config(target: &SshTarget, inst: &Instance) -> Result<()> {
    let ssh_config = ssh_config_path()?;

    if let Some(parent) = ssh_config.parent() {
        fs::create_dir_all(parent).context("Failed to create ~/.ssh directory")?;
    }

    let block = ssh_config_block(target, inst)?;
    let host = ssh_config_host(inst);

    let existing = if ssh_config.exists() {
        fs::read_to_string(&ssh_config).context("Failed to read ~/.ssh/config")?
    } else {
        String::new()
    };

    let cleaned = remove_named_marker_block(&existing, &host);
    let new_content = if cleaned.is_empty() {
        format!("{block}\n")
    } else {
        format!("{cleaned}\n{block}\n")
    };

    atomic_write(&ssh_config, &new_content).context("Failed to write ~/.ssh/config")?;

    tracing::info!("Updated SSH config at {}", ssh_config.display());
    Ok(())
}

/// Remove all coop marker blocks from SSH config.
fn remove_marker_blocks(content: &str) -> String {
    let mut result = String::new();
    let mut in_block = false;

    for line in content.lines() {
        if line.trim().starts_with(MARKER_PREFIX) {
            in_block = true;
            continue;
        }
        if line.trim() == MARKER_END {
            in_block = false;
            continue;
        }
        if !in_block {
            result.push_str(line);
            result.push('\n');
        }
    }

    let trimmed = result.trim_end_matches('\n');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

/// Remove the marker block for a specific instance host name.
fn remove_named_marker_block(content: &str, host: &str) -> String {
    let target_marker = format!("{MARKER_PREFIX} {host}");
    let mut result = String::new();
    let mut in_block = false;

    for line in content.lines() {
        if line.trim() == target_marker {
            in_block = true;
            continue;
        }
        if in_block && line.trim() == MARKER_END {
            in_block = false;
            continue;
        }
        if !in_block {
            result.push_str(line);
            result.push('\n');
        }
    }

    let trimmed = result.trim_end_matches('\n');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

const CODE_INSTALL_HINT: &str =
    "To install the `code` CLI: open VS Code, Cmd+Shift+P, 'Shell Command: Install'";
const ZED_INSTALL_HINT: &str = "To install the `zed` CLI: open Zed, Cmd+Shift+P, 'cli: install'";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NonzeroExit {
    EditorFailure,
    LauncherMiss,
}

struct LaunchStrategy {
    editor: EditorKind,
    nonzero_exit: NonzeroExit,
    name: &'static str,
    cmd: String,
    args: Vec<String>,
}

/// Bytes escaped before a guest path is interpolated into an editor URL.
/// `--project` is only checked for a leading `/` ([`GuestPath::absolute`]), so
/// the path can carry URL-significant bytes: unescaped, `#` reads as a fragment
/// and silently truncates the target. `%` is escaped too, so a path that merely
/// looks pre-encoded is not decoded a second time.
const URL_PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

fn vscode_strategies(host: &str, remote_path: &GuestPath) -> Vec<LaunchStrategy> {
    let remote_arg = format!("ssh-remote+{host}");
    let path_arg = remote_path.to_string();
    let mut strategies = vec![LaunchStrategy {
        editor: EditorKind::Code,
        nonzero_exit: NonzeroExit::EditorFailure,
        name: "code CLI",
        cmd: "code".into(),
        args: vec!["--remote".into(), remote_arg.clone(), path_arg.clone()],
    }];
    if cfg!(target_os = "macos") {
        strategies.push(LaunchStrategy {
            editor: EditorKind::Code,
            nonzero_exit: NonzeroExit::LauncherMiss,
            name: "macOS open -a 'Visual Studio Code'",
            cmd: "open".into(),
            args: vec![
                "-a".into(),
                "Visual Studio Code".into(),
                "--args".into(),
                "--remote".into(),
                remote_arg,
                path_arg,
            ],
        });
    }
    strategies
}

fn zed_strategies(host: &str, remote_path: &GuestPath) -> Vec<LaunchStrategy> {
    // Zed shells out to the system `ssh`, so the `coop-<name>` alias in
    // ~/.ssh/config supplies host, port, user, and key. Unlike the VS Code
    // strategies (argv), the path lands in a URL, so it needs escaping.
    let path = utf8_percent_encode(remote_path.as_ref(), URL_PATH).to_string();
    let mut strategies = vec![LaunchStrategy {
        editor: EditorKind::Zed,
        nonzero_exit: NonzeroExit::EditorFailure,
        name: "zed CLI",
        cmd: "zed".into(),
        args: vec![format!("ssh://{host}{path}")],
    }];
    if cfg!(target_os = "macos") {
        strategies.push(LaunchStrategy {
            editor: EditorKind::Zed,
            nonzero_exit: NonzeroExit::LauncherMiss,
            name: "macOS open zed:// URL",
            cmd: "open".into(),
            args: vec![format!("zed://ssh/{host}{path}")],
        });
    }
    strategies
}

/// Launch order for `editor`: an explicit choice pins its strategies;
/// no choice tries VS Code first, then falls through to Zed.
fn editor_strategies(
    editor: Option<EditorKind>,
    host: &str,
    remote_path: &GuestPath,
) -> Vec<LaunchStrategy> {
    match editor {
        Some(EditorKind::Code) => vscode_strategies(host, remote_path),
        Some(EditorKind::Zed) => zed_strategies(host, remote_path),
        None => {
            let mut strategies = vscode_strategies(host, remote_path);
            strategies.extend(zed_strategies(host, remote_path));
            strategies
        }
    }
}

/// CLI-install hints to print when every strategy for `editor` missed.
fn install_hints(editor: Option<EditorKind>) -> &'static [&'static str] {
    match editor {
        Some(EditorKind::Code) => &[CODE_INSTALL_HINT],
        Some(EditorKind::Zed) => &[ZED_INSTALL_HINT],
        None => &[CODE_INSTALL_HINT, ZED_INSTALL_HINT],
    }
}

/// A non-zero editor exit means it declined the request. Its remaining launch
/// strategies may still recover, but a different editor must not open
/// unexpectedly. Spawn failures and launcher misses do not set `failed_editor`,
/// so auto detection can continue when an editor is merely unavailable.
fn may_try_after_nonzero_exit(failed_editor: Option<EditorKind>, candidate: EditorKind) -> bool {
    failed_editor.is_none_or(|editor| editor == candidate)
}

fn launch_editor(
    inst: &Instance,
    remote_path: &GuestPath,
    editor: Option<EditorKind>,
) -> Result<()> {
    let host = ssh_config_host(inst);
    let strategies = editor_strategies(editor, &host, remote_path);

    let mut tried = Vec::new();
    let mut failed_editor = None;
    for strategy in &strategies {
        if !may_try_after_nonzero_exit(failed_editor, strategy.editor) {
            break;
        }
        tracing::info!(
            "Trying {}: {} {}",
            strategy.name,
            strategy.cmd,
            strategy.args.join(" ")
        );
        match Command::new(&strategy.cmd).args(&strategy.args).status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                tracing::debug!("{} exited with {status}", strategy.name);
                tried.push(format!("{} exited with {status}", strategy.name));
                if strategy.nonzero_exit == NonzeroExit::EditorFailure {
                    failed_editor = Some(strategy.editor);
                }
            }
            // Every spawn failure counts as a miss, not a hard stop: in the
            // auto-detect chain an unusable earlier binary (missing, or present
            // but not executable) must not shadow a working later editor.
            Err(e) => {
                tracing::debug!("{}: {e}", strategy.name);
                tried.push(format!("{} ({e})", strategy.name));
            }
        }
    }

    let hints = install_hints(editor);
    bail!(
        "Could not open an editor. Tried:\n{}\n\n{}",
        tried
            .iter()
            .map(|n| format!("  - {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
        hints.join("\n")
    );
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use std::num::{NonZeroU16, NonZeroU32};
    use std::process::Stdio;

    use super::*;
    use crate::backend::{Hostname, SshTarget, SshUser};
    use crate::config::{ImageName, InstanceIndex, InstanceName};
    use proptest::prelude::*;

    fn temp_instance(dir: &Path) -> Instance {
        Instance {
            name: InstanceName::new("test").expect("valid name"),
            index: InstanceIndex::new(0).expect("0 is in range"),
            dir: dir.to_path_buf(),
            image: ImageName::new("default").expect("valid image name"),
            firecracker_control: crate::config::FirecrackerControlMode::LegacyTcp,
        }
    }

    fn test_pull_operation(parent: &Path) -> PullOperation {
        PullOperation::new(parent.join("pull-staging/work")).unwrap()
    }

    fn assert_writer_child_holds_pull_lock(
        operation: PullOperation,
        child: &mut std::process::Child,
        lock_path: &Path,
    ) {
        use std::os::fd::AsRawFd as _;

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            child.try_wait().unwrap().is_none(),
            "writer exited too early"
        );
        drop(operation);

        let probe = fs::OpenOptions::new().write(true).open(lock_path).unwrap();
        // SAFETY: probe owns a valid descriptor for the duration of the call.
        let lock_result = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        let lock_error = std::io::Error::last_os_error();
        let process_group = i32::try_from(child.id()).unwrap();
        // SAFETY: the fixture puts the writer in a process group whose ID is
        // the child's PID. Killing the group also terminates rsync helpers that
        // inherited the lock descriptor.
        assert_eq!(unsafe { libc::kill(-process_group, libc::SIGKILL) }, 0);
        child.wait().unwrap();
        assert_eq!(
            lock_result, -1,
            "writer child did not retain the pull operation lock"
        );
        assert_eq!(lock_error.kind(), std::io::ErrorKind::WouldBlock);

        let mut released = false;
        for _ in 0..50 {
            // SAFETY: probe owns a valid descriptor for every attempt.
            if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            released,
            "writer process group did not release the pull lock"
        );
    }

    fn assert_missing_guest_rsync_stops_transfer(operation: &str) {
        let temp = crate::host_tool::trusted_test_tempdir();
        let root = temp.path();
        let capture = root.join("calls");
        let ssh = root.join("ssh");
        fs::write(&ssh, format!(
            "#!/bin/sh\nfor last do :; done\nprintf '%s\\n' \"$last\" >> {}\ncase \"$last\" in\n *'which rsync'*|*'command -v rsync'*) exit 127;;\n *'tar xf'*) cat >/dev/null;;\n *) exit 0;;\nesac\n",
            crate::shell::shell_escape(capture.to_str().unwrap()),
        )).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        let rsync = root.join("rsync");
        fs::write(
            &rsync,
            format!(
                "#!/bin/sh\nprintf 'rsync-transfer\\n' >> {}\n",
                crate::shell::shell_escape(capture.to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&rsync, fs::Permissions::from_mode(0o755)).unwrap();
        let source = root.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("keep"), "content").unwrap();
        let inst = temp_instance(&root.join("instance"));
        let guest = GuestPath::absolute("/workspace").unwrap();
        WorkspaceState {
            guest_path: guest.clone(),
            source: WorkspaceSource::Workspace {
                host_path: source.clone(),
            },
        }
        .save(&inst)
        .unwrap();
        let target = fake_ssh_target().with_host_tools(
            crate::config::SshHostToolsConfig::with_exact_paths(Some(&ssh), None, Some(&rsync))
                .unwrap(),
        );
        let running = RunningInstance::new(inst, target);
        let result = match operation {
            "copy" => copy_workspace(running.target(), &source, false),
            "push" => push(&running, None, true, false),
            "pull" => pull(&running, Some(root.join("dest").to_str().unwrap()), true),
            "mount" => sync_mount_contents(
                running.target(),
                &[Mount {
                    host_path: source,
                    guest_path: guest,
                }],
                false,
            ),
            _ => unreachable!(),
        };
        let error = result.expect_err("transfer must require guest rsync");
        assert!(format!("{error:#}").contains("rsync"), "{error:#}");
        let calls = fs::read_to_string(capture).unwrap();
        assert!(calls.contains("command -v rsync"), "{calls}");
        assert!(!calls.contains("tar "), "{calls}");
        assert!(!calls.contains("rsync-transfer"), "{calls}");
    }

    #[test]
    fn missing_guest_rsync_stops_initial_copy() {
        assert_missing_guest_rsync_stops_transfer("copy");
    }

    #[test]
    fn missing_guest_rsync_stops_push() {
        assert_missing_guest_rsync_stops_transfer("push");
    }

    #[test]
    fn missing_guest_rsync_stops_pull() {
        assert_missing_guest_rsync_stops_transfer("pull");
    }

    #[test]
    fn missing_guest_rsync_stops_mount_sync() {
        assert_missing_guest_rsync_stops_transfer("mount");
    }

    #[test]
    fn copy_and_push_share_filters_but_only_push_deletes() {
        let temp = crate::host_tool::trusted_test_tempdir();
        let root = temp.path();
        let ssh = root.join("ssh");
        fs::write(&ssh, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        let capture = root.join("argv");
        let rsync = root.join("rsync");
        fs::write(
            &rsync,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > {}\n",
                crate::shell::shell_escape(capture.to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&rsync, fs::Permissions::from_mode(0o755)).unwrap();
        let target = fake_ssh_target().with_host_tools(
            crate::config::SshHostToolsConfig::with_exact_paths(Some(&ssh), None, Some(&rsync))
                .unwrap(),
        );
        let source = root.join("source with spaces");
        fs::create_dir(&source).unwrap();
        let read_args = || {
            fs::read(&capture)
                .unwrap()
                .split(|b| *b == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
                .collect::<Vec<_>>()
        };
        for exclude_git in [false, true] {
            copy_workspace(&target, &source, exclude_git).unwrap();
            let copied = read_args();
            assert!(!copied.iter().any(|arg| arg == "--delete"));
            assert_eq!(
                &copied[..copied.len() - 2],
                rsync_base_args(&target, exclude_git).unwrap()
            );
            rsync_push(&target, &source, &default_workspace_path(), exclude_git).unwrap();
            let mirrored = read_args();
            assert!(mirrored.iter().any(|arg| arg == "--delete"));
            assert_eq!(
                copied,
                mirrored
                    .into_iter()
                    .filter(|arg| arg != "--delete")
                    .collect::<Vec<_>>()
            );
        }
        let custom = GuestPath::absolute("/custom workspace").unwrap();
        rsync_push(&target, &source, &custom, true).unwrap();
        assert_eq!(
            read_args().last().unwrap(),
            &format!("{}:{custom}/", target.addr())
        );
    }

    #[test]
    fn rsync_default_exclusions_are_directory_only() {
        use std::os::unix::fs::symlink;

        let temp = crate::test_support::private_tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("dest");
        fs::create_dir_all(source.join("files")).unwrap();
        fs::create_dir_all(source.join("links")).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("keep"), "keep").unwrap();
        for exclude in DEFAULT_EXCLUDES {
            let name = exclude.trim_end_matches('/');
            for parent in ["", "nested"] {
                let dir = source.join(parent).join(name);
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("marker"), "excluded").unwrap();
            }
            fs::write(source.join("files").join(name), "ordinary file").unwrap();
            symlink("../nested", source.join("links").join(name)).unwrap();
            fs::write(source.join(format!("{name}.txt")), "near miss").unwrap();
        }
        let target = fake_ssh_target();
        let status = target
            .rsync_command()
            .unwrap()
            .args(rsync_base_args(&target, true).unwrap())
            .arg(format!("{}/", source.display()))
            .arg(format!("{}/", dest.display()))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read_to_string(dest.join("keep")).unwrap(), "keep");
        for exclude in DEFAULT_EXCLUDES {
            let name = exclude.trim_end_matches('/');
            assert!(!dest.join(name).exists());
            assert!(!dest.join("nested").join(name).exists());
            assert_eq!(
                fs::read_to_string(dest.join("files").join(name)).unwrap(),
                "ordinary file"
            );
            assert_eq!(
                fs::read_link(dest.join("links").join(name)).unwrap(),
                Path::new("../nested")
            );
            assert!(dest.join(format!("{name}.txt")).is_file());
        }
    }

    #[test]
    fn failed_or_dirty_status_prevents_transfer() {
        use std::os::unix::fs::PermissionsExt as _;

        let Ok(mode) = std::env::var("COOP_TEST_CLEANLINESS_MODE") else {
            for mode in [
                "guest_git_failure",
                "guest_git_file_failure",
                "ssh_failure",
                "guest_dirty",
                "clean",
                "force",
            ] {
                let temp = crate::host_tool::trusted_test_tempdir();
                let ssh = temp.path().join("ssh");
                let capture = temp.path().join("ssh-calls");
                let script = format!(
                    "#!/bin/sh\nCOOP_TEST_CLEANLINESS_MODE={}\nCOOP_TEST_CAPTURE={}\nPATH={}:$PATH\nexport COOP_TEST_CLEANLINESS_MODE COOP_TEST_CAPTURE PATH\nprintf '%s\\n' \"$*\" >> \"$COOP_TEST_CAPTURE\"\ncase \"$COOP_TEST_CLEANLINESS_MODE\" in\n guest_git_failure|guest_git_file_failure) for last; do :; done; exec sh -c \"$last\";;\n ssh_failure) exit 255;;\n guest_dirty) echo ' M guest-file'; exit 0;;\n clean|force) exit 0;;\nesac\n",
                    crate::shell::shell_escape(mode),
                    crate::shell::shell_escape(&capture.to_string_lossy()),
                    crate::shell::shell_escape(&temp.path().to_string_lossy()),
                );
                fs::write(&ssh, script).unwrap();
                fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
                let rsync = temp.path().join("rsync");
                fs::write(
                    &rsync,
                    format!(
                        "#!/bin/sh\nprintf 'rsync-transfer\\n' >> {}\n",
                        crate::shell::shell_escape(capture.to_str().unwrap())
                    ),
                )
                .unwrap();
                fs::set_permissions(&rsync, fs::Permissions::from_mode(0o755)).unwrap();
                let git = temp.path().join("git");
                fs::write(&git, "#!/bin/sh\ncase \"$COOP_TEST_CLEANLINESS_MODE\" in\n guest_git_failure|guest_git_file_failure) exit 42;;\nesac\n").unwrap();
                fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
                let path = format!(
                    "{}:{}",
                    temp.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                );
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "workspace::tests::failed_or_dirty_status_prevents_transfer",
                    ])
                    .env("COOP_TEST_CLEANLINESS_MODE", mode)
                    .env("COOP_TEST_CLEANLINESS_ROOT", temp.path())
                    .env("COOP_TEST_CAPTURE", &capture)
                    .env("PATH", path)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{mode}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let calls = fs::read_to_string(temp.path().join("ssh-calls")).unwrap_or_default();
                if mode == "clean" || mode == "force" {
                    assert!(calls.contains("rsync-transfer"), "{calls}");
                    if mode == "force" {
                        assert!(!calls.contains("git status"), "{calls}");
                    }
                } else {
                    assert!(!calls.contains("rsync-transfer"), "{mode}: {calls}");
                    assert!(!calls.contains("command -v rsync"), "{mode}: {calls}");
                }
            }
            return;
        };

        let root = PathBuf::from(std::env::var("COOP_TEST_CLEANLINESS_ROOT").unwrap());
        run_cleanliness_case(&root, &mode);
    }

    fn run_cleanliness_case(root: &Path, mode: &str) {
        let source = root.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), "content").unwrap();
        let guest = root.join("guest");
        fs::create_dir(&guest).unwrap();
        if mode == "guest_git_file_failure" {
            fs::write(guest.join(".git"), "gitdir: /tmp/worktree\n").unwrap();
        } else {
            fs::create_dir_all(guest.join(".git")).unwrap();
        }
        let inst = temp_instance(&root.join("instance"));
        WorkspaceState {
            guest_path: GuestPath::absolute(guest.to_string_lossy().into_owned()).unwrap(),
            source: WorkspaceSource::Workspace {
                host_path: source.clone(),
            },
        }
        .save(&inst)
        .unwrap();
        let target = SshTarget::tcp(
            Hostname::new("localhost").unwrap(),
            NonZeroU16::new(2222).unwrap(),
            SshUser::new("ubuntu").unwrap(),
            root.join("key"),
            crate::config::SshHostToolsConfig::with_exact_paths(
                Some(&root.join("ssh")),
                None,
                Some(&root.join("rsync")),
            )
            .unwrap(),
        );
        let running = RunningInstance::new(inst, target);
        let result = push(&running, None, mode == "force", false);
        match mode {
            "clean" | "force" => assert!(result.is_ok(), "{result:?}"),
            "guest_git_failure" | "guest_git_file_failure" => assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("guest git status failed")
            ),
            "ssh_failure" => assert!(result.unwrap_err().to_string().contains("SSH or guest")),
            "guest_dirty" => {
                let message = result.unwrap_err().to_string();
                assert!(message.contains("Guest workspace has changes"), "{message}");
                assert!(
                    message.contains("coop pull --dir <new-directory>"),
                    "{message}"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn vscode_strategies_first_is_code_cli() {
        let path = GuestPath::new("/workspace");
        let strategies = vscode_strategies("coop-test", &path);
        // The `code` CLI strategy is present on every platform (the macOS
        // `open -a` fallback is appended after it).
        let code = &strategies[0];
        assert_eq!(code.name, "code CLI");
        assert_eq!(code.cmd, "code");
        assert_eq!(code.nonzero_exit, NonzeroExit::EditorFailure);
        let args: Vec<&str> = code.args.iter().map(String::as_str).collect();
        assert_eq!(args, ["--remote", "ssh-remote+coop-test", "/workspace"]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn vscode_strategies_macos_launcher_nonzero_is_a_miss() {
        let strategies = vscode_strategies("coop-test", &GuestPath::new("/workspace"));
        assert_eq!(strategies[1].nonzero_exit, NonzeroExit::LauncherMiss);
    }

    #[test]
    fn zed_strategies_first_is_zed_cli() {
        let path = GuestPath::new("/workspace");
        let strategies = zed_strategies("coop-test", &path);
        // The `zed` CLI strategy is present on every platform (the macOS
        // `open zed://` fallback is appended after it).
        let zed = &strategies[0];
        assert_eq!(zed.name, "zed CLI");
        assert_eq!(zed.cmd, "zed");
        assert_eq!(zed.nonzero_exit, NonzeroExit::EditorFailure);
        let args: Vec<&str> = zed.args.iter().map(String::as_str).collect();
        assert_eq!(args, ["ssh://coop-test/workspace"]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn zed_strategies_macos_fallback_is_open_zed_url() {
        let path = GuestPath::new("/workspace");
        let strategies = zed_strategies("coop-test", &path);
        // Different scheme *and* shape from the CLI arm (`zed://ssh/<host><path>`
        // vs `ssh://<host><path>`), so it needs its own assertion.
        let open = &strategies[1];
        assert_eq!(open.cmd, "open");
        assert_eq!(open.nonzero_exit, NonzeroExit::LauncherMiss);
        let args: Vec<&str> = open.args.iter().map(String::as_str).collect();
        assert_eq!(args, ["zed://ssh/coop-test/workspace"]);
    }

    #[test]
    fn zed_strategies_escape_url_significant_path_bytes() {
        // `#` would otherwise read as a fragment and open `/a` instead.
        let strategies = zed_strategies("coop-test", &GuestPath::new("/a#b c%d?e"));
        assert_eq!(strategies[0].args, ["ssh://coop-test/a%23b%20c%25d%3Fe"]);
    }

    #[test]
    fn editor_strategies_explicit_choice_pins_editor() {
        let path = GuestPath::new("/workspace");
        let code = editor_strategies(Some(EditorKind::Code), "coop-test", &path);
        assert_eq!(code[0].cmd, "code");
        assert!(code.iter().all(|s| s.editor == EditorKind::Code));
        let zed = editor_strategies(Some(EditorKind::Zed), "coop-test", &path);
        assert_eq!(zed[0].cmd, "zed");
        assert!(zed.iter().all(|s| s.editor == EditorKind::Zed));
    }

    #[test]
    fn nonzero_exit_only_allows_same_editor_fallbacks() {
        assert!(may_try_after_nonzero_exit(None, EditorKind::Zed));
        assert!(may_try_after_nonzero_exit(
            Some(EditorKind::Code),
            EditorKind::Code
        ));
        assert!(!may_try_after_nonzero_exit(
            Some(EditorKind::Code),
            EditorKind::Zed
        ));
    }

    #[test]
    fn install_hints_match_the_pinned_editor() {
        assert_eq!(install_hints(Some(EditorKind::Code)), [CODE_INSTALL_HINT]);
        assert_eq!(install_hints(Some(EditorKind::Zed)), [ZED_INSTALL_HINT]);
        assert_eq!(install_hints(None), [CODE_INSTALL_HINT, ZED_INSTALL_HINT]);
    }

    #[test]
    fn editor_strategies_default_tries_code_before_zed() {
        let path = GuestPath::new("/workspace");
        let strategies = editor_strategies(None, "coop-test", &path);
        let code_pos = strategies.iter().position(|s| s.cmd == "code");
        let zed_pos = strategies.iter().position(|s| s.cmd == "zed");
        assert!(code_pos.expect("code present") < zed_pos.expect("zed present"));
    }

    #[test]
    fn resolve_host_dir_prefers_explicit_over_state() {
        let state = WorkspaceState {
            guest_path: GuestPath::absolute("/workspace").unwrap(),
            source: WorkspaceSource::Workspace {
                host_path: PathBuf::from("/from/state"),
            },
        };
        let dir = resolve_host_dir(Some("/from/cli"), &state, "push").expect("explicit dir");
        assert_eq!(dir, PathBuf::from("/from/cli"));
    }

    #[test]
    fn resolve_host_dir_makes_relative_transport_paths_absolute() {
        let state = WorkspaceState {
            guest_path: GuestPath::absolute("/workspace").unwrap(),
            source: WorkspaceSource::Workspace {
                host_path: PathBuf::from("/from/state"),
            },
        };
        let dir = resolve_host_dir(Some("relative-workspace"), &state, "push").unwrap();
        assert_eq!(
            dir,
            std::env::current_dir().unwrap().join("relative-workspace")
        );
    }

    #[test]
    fn resolve_host_dir_falls_back_to_state_host_path() {
        let state = WorkspaceState {
            guest_path: GuestPath::absolute("/workspace").unwrap(),
            source: WorkspaceSource::Mount {
                host_path: PathBuf::from("/from/state"),
            },
        };
        let dir = resolve_host_dir(None, &state, "push").expect("state host_path");
        assert_eq!(dir, PathBuf::from("/from/state"));
    }

    #[test]
    fn resolve_host_dir_errors_when_no_host_path_and_no_explicit() {
        let state = WorkspaceState {
            guest_path: GuestPath::absolute("/workspace").unwrap(),
            source: WorkspaceSource::GitRepo {
                url: crate::github_repo::GitRepoUrl::new("https://github.com/x/y.git"),
            },
        };
        let err = resolve_host_dir(None, &state, "push").expect_err("no host_path");
        assert!(format!("{err}").contains("No host_path"));
    }

    #[test]
    fn git_repo_rejects_extra_mount_at_workspace() {
        let tmp = crate::test_support::private_tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).expect("data");
        let mounts = vec![Mount::parse(data.to_str().unwrap()).expect("mount")];

        let err = ValidatedMounts::assemble(WorkspaceMountRule::GitRepoClone, mounts)
            .expect_err("expected /workspace collision");
        assert!(format!("{err}").contains("/workspace"));
    }

    #[test]
    fn project_mounted_rule_allows_workspace_mount() {
        let tmp = crate::test_support::private_tempdir().expect("tempdir");
        let data = tmp.path().join("proj");
        std::fs::create_dir(&data).expect("data");
        // A project legitimately mounted at /workspace (coop up --mount) is
        // fine; the ProjectMountedOrNone rule skips the collision check.
        let mounts = vec![Mount::parse(data.to_str().unwrap()).expect("mount")];
        let validated =
            ValidatedMounts::assemble(WorkspaceMountRule::ProjectMountedOrNone, mounts.clone())
                .expect("workspace mount allowed under ProjectMountedOrNone");
        assert_eq!(validated.into_vec().len(), 1);
    }

    #[test]
    fn assemble_rejects_duplicate_guest_paths_under_every_rule() {
        let tmp = crate::test_support::private_tempdir().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).expect("a");
        std::fs::create_dir(&b).expect("b");
        // Two distinct hosts, same guest path — a duplicate the uniqueness
        // check must catch regardless of the workspace rule (this is the
        // invariant quickstart previously skipped).
        let dup = || {
            vec![
                Mount::parse(&format!("{}:/data", a.to_str().unwrap())).expect("a"),
                Mount::parse(&format!("{}:/data", b.to_str().unwrap())).expect("b"),
            ]
        };
        for rule in [
            WorkspaceMountRule::CopyProject,
            WorkspaceMountRule::GitRepoClone,
            WorkspaceMountRule::ProjectMountedOrNone,
        ] {
            let err = ValidatedMounts::assemble(rule, dup()).expect_err("duplicate guest path");
            assert!(format!("{err}").contains("Duplicate mount guest path"));
        }
    }

    /// Run `f` with a thread-local WARN-level subscriber and return its
    /// result alongside the captured log output. Lets a test assert that a
    /// warning was emitted, not just that an error was swallowed.
    fn capture_warn<T>(f: impl FnOnce() -> T) -> (T, String) {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("log buffer lock")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for SharedBuf {
            type Writer = SharedBuf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(SharedBuf(Arc::clone(&buf)))
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let result = tracing::subscriber::with_default(subscriber, f);
        let logs = String::from_utf8(buf.lock().expect("log buffer lock").clone())
            .expect("log output is utf-8");
        (result, logs)
    }

    #[test]
    fn try_load_returns_none_when_missing() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let result = WorkspaceState::try_load(&inst).expect("no IO error");
        assert!(result.is_none());
    }

    #[test]
    fn try_load_errors_on_invalid_json() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        fs::write(inst.workspace_state_path(), "not json").expect("write");
        let result = WorkspaceState::try_load(&inst);
        assert!(result.is_err());
    }

    #[test]
    fn try_load_or_warn_swallows_parse_error_and_warns() {
        // `try_load_or_warn` wraps `try_load`: it must pass a present state
        // through untouched, and turn a parse error into `None` while logging
        // a WARN that names the degraded feature (`consequence`) and the
        // instance. Both arms are pinned here so a mutant that drops either
        // the pass-through or the warning is caught.
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());

        // Present: the loaded state flows through unchanged, no warning.
        let saved = WorkspaceState {
            guest_path: GuestPath::absolute("/workspace").unwrap(),
            source: WorkspaceSource::Workspace {
                host_path: PathBuf::from("/host/dir"),
            },
        };
        saved.save(&inst).expect("save");
        let (loaded, ok_logs) =
            capture_warn(|| try_load_or_warn(&inst, "token forwarding skipped"));
        let loaded = loaded.expect("present state must pass through");
        assert!(matches!(
            loaded.source,
            WorkspaceSource::Workspace { ref host_path } if host_path == Path::new("/host/dir")
        ));
        assert!(
            !ok_logs.contains("WARN"),
            "successful load must not warn: {ok_logs}"
        );

        // Parse error: swallowed to `None`, with an actionable warning.
        fs::write(inst.workspace_state_path(), "not json").expect("write");
        let (result, logs) = capture_warn(|| try_load_or_warn(&inst, "token forwarding skipped"));

        assert!(result.is_none(), "parse error must not propagate");
        assert!(
            logs.contains("token forwarding skipped"),
            "warning must name the degraded feature: {logs}"
        );
        assert!(
            logs.contains(&inst.name.to_string()),
            "warning must name the instance: {logs}"
        );
    }

    #[test]
    fn load_or_default_uses_saved_state() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let state = WorkspaceState {
            guest_path: GuestPath::absolute("/custom").unwrap(),
            source: WorkspaceSource::GitRepo {
                url: crate::github_repo::GitRepoUrl::new("https://github.com/x/y.git"),
            },
        };
        state.save(&inst).expect("save");
        let loaded = load_or_default(&inst, None, "push").expect("load");
        assert_eq!(loaded.guest_path.to_string(), "/custom");
        assert!(matches!(
            loaded.source,
            WorkspaceSource::GitRepo { ref url } if url.as_str() == "https://github.com/x/y.git"
        ));
    }

    #[test]
    fn load_or_default_falls_back_with_dir_arg() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        // No workspace.json exists; the synthetic state carries the
        // explicit --dir as its host_path so consumers can read it
        // uniformly.
        let loaded = load_or_default(&inst, Some("./project"), "push").expect("load");
        assert_eq!(loaded.guest_path.to_string(), GUEST_WORKSPACE);
        assert!(matches!(
            loaded.source,
            WorkspaceSource::Workspace { ref host_path } if host_path == Path::new("./project")
        ));
    }

    #[test]
    fn record_mount_state_persists_first_mount() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let primary = crate::test_support::private_tempdir().expect("primary");
        let secondary = crate::test_support::private_tempdir().expect("secondary");
        let mounts = vec![
            crate::config::Mount {
                host_path: primary.path().to_path_buf(),
                guest_path: default_workspace_path(),
            },
            crate::config::Mount {
                host_path: secondary.path().to_path_buf(),
                guest_path: crate::paths::GuestPath::absolute("/data").unwrap(),
            },
        ];
        record_mount_state(&inst, &mounts).expect("save");
        let loaded = WorkspaceState::try_load(&inst)
            .expect("no IO error")
            .expect("state should exist");
        assert!(matches!(
            loaded.source,
            WorkspaceSource::Mount { ref host_path } if host_path == primary.path()
        ));
        assert_eq!(loaded.guest_path.to_string(), "/workspace");
    }

    #[test]
    fn record_mount_state_noop_on_empty() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        record_mount_state(&inst, &[]).expect("ok");
        assert!(
            WorkspaceState::try_load(&inst)
                .expect("no IO error")
                .is_none(),
            "empty mounts should not write state"
        );
    }

    fn arb_mount() -> impl Strategy<Value = crate::config::Mount> {
        // `record_mount_state` copies fields verbatim; it never touches the
        // filesystem, so arbitrary (but absolute) paths are fine here.
        ("/[A-Za-z0-9_/]{0,16}", "[A-Za-z0-9_/.-]{0,16}").prop_map(|(guest, host)| {
            crate::config::Mount {
                host_path: PathBuf::from(host),
                guest_path: GuestPath::absolute(format!("/m{guest}")).expect("absolute"),
            }
        })
    }

    fn read_state_json(inst: &Instance) -> String {
        fs::read_to_string(inst.workspace_state_path()).expect("state file present")
    }

    proptest! {
        /// Recording mount state is an idempotent overwrite: recording the
        /// same primary mount twice yields the same on-disk bytes, and
        /// recording a different mount replaces the previous one wholesale.
        #[test]
        fn record_mount_state_is_idempotent_and_replacing(
            m1 in arb_mount(),
            m2 in arb_mount(),
        ) {
            let dir = crate::test_support::private_tempdir().expect("tempdir");
            let inst = temp_instance(dir.path());

            record_mount_state(&inst, std::slice::from_ref(&m1)).expect("record m1");
            let after_first = read_state_json(&inst);
            record_mount_state(&inst, std::slice::from_ref(&m1)).expect("record m1 again");
            prop_assert_eq!(&after_first, &read_state_json(&inst));

            record_mount_state(&inst, std::slice::from_ref(&m2)).expect("record m2");
            let replaced = WorkspaceState::try_load(&inst)
                .expect("no IO error")
                .expect("state present");
            prop_assert_eq!(&replaced.guest_path, &m2.guest_path);
            prop_assert_eq!(replaced.source.host_path(), Some(m2.host_path.as_path()));
        }
    }

    #[test]
    fn load_or_default_errors_without_dir_or_state() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let result = load_or_default(&inst, None, "pull");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("No workspace.json found"), "unexpected: {msg}");
        assert!(msg.contains("coop pull"), "should mention command: {msg}");
    }

    #[test]
    fn remove_all_blocks() {
        let input = "\
Host other\n\
    HostName 1.2.3.4\n\
# coop START coop-0\n\
Host coop-0\n\
    HostName 172.16.0.2\n\
# coop END\n\
Host another\n\
    HostName 5.6.7.8\n\
# coop START coop-1\n\
Host coop-1\n\
    HostName 172.16.0.3\n\
# coop END\n";

        let result = remove_marker_blocks(input);
        assert!(!result.contains("coop-0"));
        assert!(!result.contains("coop-1"));
        assert!(result.contains("Host other"));
        assert!(result.contains("Host another"));
    }

    #[test]
    fn remove_named_block_leaves_others() {
        let input = "\
# coop START coop-a\n\
Host coop-a\n\
    HostName 172.16.0.2\n\
# coop END\n\
# coop START coop-b\n\
Host coop-b\n\
    HostName 172.16.0.3\n\
# coop END\n";

        let result = remove_named_marker_block(input, "coop-a");
        assert!(!result.contains("coop-a"));
        assert!(result.contains("coop-b"));
        assert!(result.contains("172.16.0.3"));
    }

    #[test]
    fn remove_named_block_noop_when_missing() {
        let input = "Host something\n    HostName 1.2.3.4\n";
        let result = remove_named_marker_block(input, "coop-x");
        assert_eq!(result, input);
    }

    #[test]
    fn marker_block_present_detects_matching_host() {
        let input = "\
# coop START coop-a\n\
Host coop-a\n\
    HostName 172.16.0.2\n\
# coop END\n";
        assert!(marker_block_present(input, "coop-a"));
    }

    #[test]
    fn marker_block_present_false_for_other_host() {
        let input = "\
# coop START coop-a\n\
Host coop-a\n\
    HostName 172.16.0.2\n\
# coop END\n";
        // A different instance's block must not count as present, or
        // restart would refresh an alias the user never installed.
        assert!(!marker_block_present(input, "coop-b"));
    }

    #[test]
    fn marker_block_present_false_for_empty_config() {
        assert!(!marker_block_present("", "coop-a"));
    }

    #[test]
    fn marker_block_present_ignores_host_substring() {
        // `coop-a` is a prefix of `coop-app`; a substring match would
        // wrongly report the block present.
        let input = "\
# coop START coop-app\n\
Host coop-app\n\
    HostName 172.16.0.2\n\
# coop END\n";
        assert!(!marker_block_present(input, "coop-a"));
    }

    #[test]
    fn ssh_config_block_has_expected_fields() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let block = ssh_config_block(&fake_ssh_target(), &inst).unwrap();

        assert_eq!(
            block,
            concat!(
                "# coop START coop-test\n",
                "Host coop-test\n",
                "    HostName 127.0.0.1\n",
                "    Port 2222\n",
                "    User ubuntu\n",
                "    IdentityFile /tmp/key\n",
                "    IdentitiesOnly yes\n",
                "    IdentityAgent none\n",
                "    ForwardAgent no\n",
                "    StrictHostKeyChecking no\n",
                "    UserKnownHostsFile /dev/null\n",
                "    LogLevel ERROR\n",
                "# coop END",
            )
        );
    }

    #[test]
    fn ssh_config_block_uses_firecracker_vsock_proxy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let target = SshTarget::firecracker_vsock_with_proxy(
            PathBuf::from("/tmp/instance path/vsock.sock"),
            NonZeroU32::new(22).unwrap(),
            PathBuf::from("/opt/coop bin/coop"),
            SshUser::new("ubuntu").unwrap(),
            PathBuf::from("/tmp/key"),
            crate::config::SshHostToolsConfig::default(),
        );
        let block = ssh_config_block(&target, &inst).unwrap();

        assert!(block.contains("    HostName coop-vsock\n"));
        assert!(block.contains(
            "    ProxyCommand '/opt/coop bin/coop' __vsock-proxy --socket '/tmp/instance path/vsock.sock' --port 22\n"
        ));
        assert!(!block.contains("    Port "));
    }

    #[test]
    fn remove_all_from_only_moat_blocks_returns_empty() {
        let input = "\
# coop START coop-0\n\
Host coop-0\n\
    HostName 172.16.0.2\n\
# coop END\n";

        let result = remove_marker_blocks(input);
        assert!(result.is_empty());
    }

    #[test]
    fn remove_named_marker_block_removes_only_the_named_block() {
        // `before`, `after`, and a second coop block bracket the target so the
        // END-detection guard (`in_block && line == MARKER_END`) is exercised
        // both inside and outside a block. A `&&`→`||` mutant would drop the
        // surviving block's END line; a `==`→`!=` mutant would leave the
        // target block's body behind. Asserting the exact result kills both.
        let input = "\
Host before\n\
    HostName 1.1.1.1\n\
# coop START coop-target\n\
Host coop-target\n\
    HostName 2.2.2.2\n\
# coop END\n\
Host after\n\
    HostName 3.3.3.3\n\
# coop START coop-other\n\
Host coop-other\n\
    HostName 4.4.4.4\n\
# coop END\n";

        let result = remove_named_marker_block(input, "coop-target");

        let expected = "\
Host before\n\
    HostName 1.1.1.1\n\
Host after\n\
    HostName 3.3.3.3\n\
# coop START coop-other\n\
Host coop-other\n\
    HostName 4.4.4.4\n\
# coop END\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn remove_all_ssh_config_at_strips_coop_keeps_rest() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let path = dir.path().join("config");
        let original = "\
Host keep\n\
    HostName 9.9.9.9\n\
# coop START coop-test\n\
Host coop-test\n\
    HostName 172.16.0.2\n\
# coop END\n";
        std::fs::write(&path, original).expect("write config");

        remove_all_ssh_config_at(&path).expect("remove all");

        let rewritten = std::fs::read_to_string(&path).expect("read back");
        assert!(
            rewritten.contains("Host keep"),
            "non-coop block dropped: {rewritten}"
        );
        assert!(
            !rewritten.contains("coop-test"),
            "coop block survived removal: {rewritten}"
        );
    }

    #[test]
    fn remove_all_ssh_config_at_leaves_unrelated_file_byte_identical() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let path = dir.path().join("config");
        let original = "Host keep\n    HostName 9.9.9.9\n";
        std::fs::write(&path, original).expect("write config");

        remove_all_ssh_config_at(&path).expect("remove all");

        let rewritten = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            rewritten, original,
            "file with no coop block must be untouched"
        );
    }

    #[test]
    fn remove_ssh_config_at_removes_only_target_instance() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let path = dir.path().join("config");
        let inst = temp_instance(dir.path()); // host coop-test
        let original = "\
# coop START coop-test\n\
Host coop-test\n\
    HostName 172.16.0.2\n\
# coop END\n\
# coop START coop-other\n\
Host coop-other\n\
    HostName 172.16.0.3\n\
# coop END\n";
        std::fs::write(&path, original).expect("write config");

        remove_ssh_config_at(&path, &inst).expect("remove one");

        let rewritten = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !rewritten.contains("coop-test"),
            "target instance block survived: {rewritten}"
        );
        assert!(
            rewritten.contains("# coop START coop-other"),
            "other instance block was dropped: {rewritten}"
        );
        assert!(
            rewritten.contains("HostName 172.16.0.3"),
            "other block body dropped: {rewritten}"
        );
    }

    #[test]
    fn remove_ssh_config_at_leaves_file_byte_identical_when_host_absent() {
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let path = dir.path().join("config");
        let inst = temp_instance(dir.path()); // host coop-test, absent below
        let original = "\
# coop START coop-other\n\
Host coop-other\n\
    HostName 172.16.0.3\n\
# coop END\n";
        std::fs::write(&path, original).expect("write config");

        remove_ssh_config_at(&path, &inst).expect("remove one");

        let rewritten = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            rewritten, original,
            "removing an absent host must not rewrite the file"
        );
    }

    // ── exclude_git policy ────────────────────────────────────

    fn fake_ssh_target() -> SshTarget {
        use crate::backend::{Hostname, SshUser};
        SshTarget::tcp(
            Hostname::new("127.0.0.1").expect("valid host"),
            std::num::NonZeroU16::new(2222).unwrap(),
            SshUser::new("ubuntu").expect("valid user"),
            PathBuf::from("/tmp/key"),
            crate::config::SshHostToolsConfig::default(),
        )
    }

    #[test]
    fn default_excludes_omit_git() {
        // Host-to-guest transfers include Git metadata by default so agents
        // receive repository history. Pulls enforce their stricter policy at
        // the direction-specific call sites.
        assert!(
            !DEFAULT_EXCLUDES.iter().any(|e| e.contains(".git")),
            "DEFAULT_EXCLUDES must not contain a .git pattern; got {DEFAULT_EXCLUDES:?}"
        );
        assert_eq!(GIT_EXCLUDE, ".[gG][iI][tT]");
    }

    proptest! {
        /// For either `exclude_git` value, `rsync_base_args` must emit every
        /// `DEFAULT_EXCLUDES` member, choose exactly one git rule keyed on the
        /// flag, and place that rule before the `.gitignore` merge so
        /// rsync's first-match-wins ordering can't let a user's `.gitignore`
        /// strip git state. Replaces the two earlier per-flag example tests
        /// and closes the gap where membership of every exclude went
        /// unchecked.
        #[test]
        fn rsync_base_args_membership_and_git_filter_ordering(exclude_git in any::<bool>()) {
            let args = rsync_base_args(&fake_ssh_target(), exclude_git).unwrap();

            for exc in DEFAULT_EXCLUDES {
                let needle = format!("--exclude={exc}");
                prop_assert!(
                    args.iter().any(|a| a == &needle),
                    "missing default exclude {needle:?}: {args:?}"
                );
            }

            let has_protect = args.iter().any(|a| a == "--filter=+ /.git/***");
            let has_exclude_git = args
                .iter()
                .any(|a| a == "--exclude=.[gG][iI][tT]");
            // Exactly one git rule, selected by the flag.
            prop_assert_eq!(has_protect, !exclude_git);
            prop_assert_eq!(has_exclude_git, exclude_git);

            let gitignore_idx = args
                .iter()
                .position(|a| a == "--filter=:- .gitignore")
                .expect("expected .gitignore merge filter");
            let git_rule_idx = args
                .iter()
                .position(|a| {
                    a == "--filter=+ /.git/***" || a == "--exclude=.[gG][iI][tT]"
                })
                .expect("expected a git rule");
            prop_assert!(
                git_rule_idx < gitignore_idx,
                "git rule must precede the .gitignore merge: {:?}",
                args
            );
        }
    }

    #[test]
    fn pull_transports_apply_best_effort_git_exclusions() {
        let target = fake_ssh_target();
        let rsync_args = rsync_pull_args(&target).unwrap();
        assert!(
            rsync_args
                .iter()
                .any(|arg| arg == "--exclude=.[gG][iI][tT]")
        );
        assert!(!rsync_args.iter().any(|arg| arg == "--filter=+ /.git/***"));

        let staged_args =
            install_staged_pull_args(Path::new("/pull-staging"), Path::new("/host/workspace"));
        assert!(
            staged_args
                .iter()
                .any(|arg| arg == "--exclude=.[gG][iI][tT]"),
            "{staged_args:?}"
        );
    }

    #[test]
    fn pull_staging_cleanup_repairs_guest_directory_modes_without_following_symlinks() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let parent = crate::test_support::private_tempdir().unwrap();
        let outside = crate::test_support::private_tempdir().unwrap();
        let operation = test_pull_operation(parent.path());
        let mut staging = operation.staging().unwrap();
        let staging_path = staging.path().unwrap().to_path_buf();
        let locked = staging.path().unwrap().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("guest-file"), "untrusted").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        symlink(outside.path(), staging.path().unwrap().join("outside-link")).unwrap();
        fs::set_permissions(outside.path(), fs::Permissions::from_mode(0o000)).unwrap();

        let result = staging.finish(Err(anyhow::anyhow!("receive failed")));

        assert_eq!(result.unwrap_err().to_string(), "receive failed");
        assert!(!staging_path.exists());
        assert_eq!(
            fs::symlink_metadata(outside.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o000,
            "cleanup must not follow a staged symlink"
        );
        fs::set_permissions(outside.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn pull_staging_path_is_a_stable_private_storage_sibling() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let instances = temp.path().join("instances");
        let inst = temp_instance(&instances.join("test"));

        assert_eq!(
            pull_staging_path(&inst).unwrap(),
            temp.path().join("pull-staging/work")
        );
    }

    #[test]
    fn pull_staging_outer_directory_stays_private_under_permissive_umask() {
        use std::os::unix::fs::PermissionsExt as _;

        const CHILD: &str = "COOP_PULL_STAGING_PRIVATE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "workspace::tests::pull_staging_outer_directory_stays_private_under_permissive_umask",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        // SAFETY: only this isolated child test changes the process umask.
        unsafe {
            libc::umask(0);
        }
        let parent = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let operation = test_pull_operation(parent.path());
        let mut staging = operation.staging().unwrap();
        fs::set_permissions(staging.path().unwrap(), fs::Permissions::from_mode(0o777)).unwrap();

        let outer = parent.path().join("pull-staging/work");
        assert_eq!(
            fs::metadata(&outer).unwrap().permissions().mode() & 0o777,
            0o700,
            "guest-controlled modes below staging must not expose its outer boundary"
        );
        staging.finish(Ok(())).unwrap();
    }

    #[test]
    fn pull_staging_replaces_the_bounded_tree_left_by_an_interrupted_pull() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent = crate::test_support::private_tempdir().unwrap();
        let root = parent.path().join("pull-staging/work");
        let stale = root.join("contents/locked");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("guest-file"), "stale").unwrap();
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o000)).unwrap();

        let operation = test_pull_operation(parent.path());
        let mut staging = operation.staging().unwrap();

        assert_eq!(staging.path().unwrap(), root.join("contents"));
        assert_eq!(fs::read_dir(staging.path().unwrap()).unwrap().count(), 0);
        staging.finish(Ok(())).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn pull_staging_cleanup_propagates_inspection_errors_other_than_missing() {
        let parent = crate::test_support::private_tempdir().unwrap();
        let non_directory = parent.path().join("file");
        fs::write(&non_directory, "not a directory").unwrap();

        let error = remove_staging_directory(&non_directory.join("child")).unwrap_err();

        assert!(
            format!("{error:#}").contains("Failed to inspect staging path"),
            "{error:#}"
        );
    }

    #[test]
    fn pull_rsync_writer_keeps_operation_lock_after_parent_guard_drops() {
        use std::os::unix::process::CommandExt as _;

        let parent = crate::test_support::private_tempdir().unwrap();
        let source = parent.path().join("source");
        let dest = parent.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("large"), vec![0; 1024 * 1024]).unwrap();

        let operation = test_pull_operation(parent.path());
        let args = vec![
            "-a".to_string(),
            "--bwlimit=1".to_string(),
            format!("{}/", source.display()),
            format!("{}/", dest.display()),
        ];
        let mut command = locked_rsync_command(&fake_ssh_target(), &args, &operation).unwrap();
        command.stdout(Stdio::null()).stderr(Stdio::null());
        command.process_group(0);
        let mut child = command.spawn().unwrap();

        assert_writer_child_holds_pull_lock(
            operation,
            &mut child,
            &parent.path().join("pull-staging/.pull.operation.lock"),
        );
    }

    #[test]
    fn pull_staging_drop_is_a_cleanup_fallback() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let parent = crate::test_support::private_tempdir().unwrap();
        let outside = crate::test_support::private_tempdir().unwrap();
        let operation = test_pull_operation(parent.path());
        let staging_path = {
            let staging = operation.staging().unwrap();
            let staging_path = staging.path().unwrap().to_path_buf();
            let locked = staging.path().unwrap().join("locked");
            fs::create_dir(&locked).unwrap();
            fs::write(locked.join("guest-file"), "untrusted").unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            symlink(outside.path(), staging.path().unwrap().join("outside-link")).unwrap();
            fs::set_permissions(outside.path(), fs::Permissions::from_mode(0o000)).unwrap();
            staging_path
        };

        assert!(!staging_path.exists());
        assert_eq!(
            fs::symlink_metadata(outside.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o000,
            "drop cleanup must not follow a staged symlink"
        );
        fs::set_permissions(outside.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn rsync_pull_stages_a_malicious_sender_before_installation() {
        use std::os::unix::fs::PermissionsExt as _;

        const ROOT_ENV: &str = "COOP_TEST_RSYNC_PULL_STAGING_ROOT";
        const REAL_RSYNC_ENV: &str = "COOP_TEST_RSYNC_PULL_REAL_RSYNC";

        let Ok(root) = std::env::var(ROOT_ENV) else {
            let temp = crate::host_tool::trusted_test_tempdir();
            let dest = temp.path().join("dest");
            let bin = temp.path().join("bin");
            let staging_parent = temp.path().join("pull-tmp");
            crate::test_support::create_private_dir_all(dest.join(".git")).unwrap();
            crate::test_support::create_private_dir_all(&bin).unwrap();
            crate::test_support::create_private_dir_all(&staging_parent).unwrap();
            fs::write(dest.join(".git/config"), "host-owned\n").unwrap();

            let real_rsync = Command::new("sh")
                .args(["-c", "command -v rsync"])
                .output()
                .unwrap();
            assert!(real_rsync.status.success());
            let real_rsync = String::from_utf8(real_rsync.stdout).unwrap();
            let real_rsync = crate::shell::shell_escape(real_rsync.trim());

            let ssh = bin.join("ssh");
            fs::write(&ssh, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
            let rsync = bin.join("rsync");
            fs::write(
                &rsync,
                format!(
                    "#!/bin/sh\n\
                 case \"$*\" in\n\
                   *:/workspace/*)\n\
                     for last do :; done\n\
                     mkdir -p \"$last/.git\"\n\
                     printf 'guest-controlled\\n' > \"$last/.git/config\"\n\
                     printf 'worktree\\n' > \"$last/workspace-file\"\n\
                     ;;\n\
                   *) exec {real_rsync} \"$@\" ;;\n\
                 esac\n"
                ),
            )
            .unwrap();
            fs::set_permissions(&rsync, fs::Permissions::from_mode(0o755)).unwrap();
            let path = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "workspace::tests::rsync_pull_stages_a_malicious_sender_before_installation",
                ])
                .env(ROOT_ENV, temp.path())
                .env(REAL_RSYNC_ENV, "/unused-explicit-environment-sentinel")
                .env("TMPDIR", &staging_parent)
                .env("PATH", path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };

        let root = PathBuf::from(root);
        let dest = root.join("dest");
        let staging = root.join("pull-tmp/staging");
        let operation = PullOperation::new(staging.clone()).unwrap();
        let target = fake_ssh_target().with_host_tools(
            crate::config::SshHostToolsConfig::with_exact_paths(
                Some(&root.join("bin/ssh")),
                None,
                Some(&root.join("bin/rsync")),
            )
            .unwrap(),
        );
        rsync_pull(
            &target,
            &GuestPath::absolute("/workspace").unwrap(),
            &dest,
            &operation,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dest.join("workspace-file")).unwrap(),
            "worktree\n"
        );
        assert_eq!(
            fs::read_to_string(dest.join(".git/config")).unwrap(),
            "host-owned\n"
        );
        assert!(!staging.exists());
    }

    #[test]
    fn staged_pull_best_effort_filter_leaves_common_host_git_path_untouched() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let staging = temp.path().join("staging");
        let dest = temp.path().join("dest");
        fs::create_dir_all(staging.join(".GiT")).unwrap();
        fs::create_dir_all(dest.join(".git")).unwrap();
        fs::write(staging.join("workspace-file"), "worktree\n").unwrap();
        fs::write(staging.join(".GiT/config"), "guest-controlled\n").unwrap();
        fs::write(dest.join(".git/config"), "host-owned\n").unwrap();

        let operation = PullOperation::new(temp.path().join("pull-operation/test")).unwrap();
        install_staged_pull(&fake_ssh_target(), &staging, &dest, &operation).unwrap();

        assert_eq!(
            fs::read_to_string(dest.join("workspace-file")).unwrap(),
            "worktree\n"
        );
        assert_eq!(
            fs::read_to_string(dest.join(".git/config")).unwrap(),
            "host-owned\n"
        );
    }

    #[test]
    fn pull_destination_requires_force_based_only_on_directory_contents() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let missing = temp.path().join("missing");
        let empty = temp.path().join("empty");
        let nonempty = temp.path().join("nonempty");
        let not_directory = temp.path().join("not-directory");
        fs::create_dir(&empty).unwrap();
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("guest-controlled"), "data").unwrap();
        fs::write(&not_directory, "data").unwrap();

        assert!(!pull_destination_is_nonempty(&missing).unwrap());
        assert!(!pull_destination_is_nonempty(&empty).unwrap());
        assert!(pull_destination_is_nonempty(&nonempty).unwrap());
        assert!(pull_destination_is_nonempty(&not_directory).is_err());
    }

    #[test]
    fn pull_warning_states_the_actual_trust_boundary() {
        assert!(PULL_UNTRUSTED_WARNING.contains("untrusted guest"));
        assert!(PULL_UNTRUSTED_WARNING.contains("potentially malicious"));
        assert!(PULL_UNTRUSTED_WARNING.contains("best effort"));
        assert!(PULL_UNTRUSTED_WARNING.contains("not a security boundary"));
    }

    // ── WorkspaceState serialization ──────────────────────────

    #[test]
    fn try_load_rejects_relative_guest_path() {
        // A hand-edited or pre-migration workspace.json with a relative
        // guest_path must be rejected at load — `GuestPath`'s transparent
        // deserialize would otherwise accept it and let it flow into the
        // remote shell commands that interpolate the path.
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        fs::write(
            inst.workspace_state_path(),
            r#"{"guest_path": "relative/dir", "source": {"kind": "workspace", "host_path": "/h"}}"#,
        )
        .expect("write");

        let err =
            WorkspaceState::try_load(&inst).expect_err("relative guest_path must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("absolute"),
            "expected absolute-path error: {msg}"
        );
    }

    #[test]
    fn try_load_legacy_shape_emits_migration_hint() {
        // The pre-#147 flat shape no longer parses; the error context
        // points the user at the remediation.
        let dir = crate::test_support::private_tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        fs::write(
            inst.workspace_state_path(),
            r#"{
                "host_path": "/legacy/project",
                "guest_path": "/workspace",
                "source": "workspace"
            }"#,
        )
        .expect("write");

        let err = WorkspaceState::try_load(&inst).expect_err("legacy shape must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("pre-#147"), "expected migration hint: {msg}");
        assert!(
            msg.contains("coop start") || msg.contains("coop destroy"),
            "expected remediation pointer: {msg}"
        );
    }

    #[test]
    fn git_repo_source_persists_url_as_bare_string() {
        // The on-disk format must not change when the url field became a
        // `GitRepoUrl` newtype: it must still serialize as the tagged
        // enum with a bare `url` string, and a file written in that exact
        // shape must still load.
        let state = WorkspaceState {
            guest_path: GuestPath::absolute(GUEST_WORKSPACE).unwrap(),
            source: WorkspaceSource::GitRepo {
                url: crate::github_repo::GitRepoUrl::new("https://github.com/x/y.git"),
            },
        };
        let value: serde_json::Value = serde_json::to_value(&state).expect("serialize to value");
        assert_eq!(value["source"]["kind"], "git_repo");
        assert_eq!(value["source"]["url"], "https://github.com/x/y.git");

        let from_disk: WorkspaceState = serde_json::from_str(&format!(
            r#"{{"guest_path": "{GUEST_WORKSPACE}", "source": {{"kind": "git_repo", "url": "https://github.com/x/y.git"}}}}"#
        ))
        .expect("load on-disk shape");
        assert!(matches!(
            from_disk.source,
            WorkspaceSource::GitRepo { ref url } if url.as_str() == "https://github.com/x/y.git"
        ));
    }
}
