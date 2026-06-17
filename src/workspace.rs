use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::backend::{RunningInstance, SshTarget};
use crate::config::{Instance, Mount};
use crate::paths::GuestPath;
use crate::remote_command::RemoteCommand;

const GUEST_WORKSPACE: &str = "/workspace";

/// Default exclusions for transfers — reproducible build/cache directories
/// only. `.git/` is intentionally absent: agents inside the guest need
/// history, branches, and the ability to make commits that survive a
/// `coop pull`. Opt out per-transfer with `exclude_git: true`.
const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules/",
    "target/",
    "__pycache__/",
    ".venv/",
    ".coop/",
];

const GIT_EXCLUDE: &str = ".git/";

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
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(
                    anyhow::anyhow!(e).context(format!("Failed to read {}", path.display()))
                );
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

/// Transfer a local directory to the guest via tar-pipe over SSH.
///
/// Streams `tar cf -` straight into a guest-side `tar xf - -C /workspace`.
/// No staging file on either side, so peak disk usage on the guest is
/// just the extracted tree. Integrity relies on SSH's MAC over the
/// localhost transport plus tar's per-header checksums.
pub fn tar_pipe_transfer(target: &SshTarget, source_dir: &Path, exclude_git: bool) -> Result<()> {
    let workspace = GuestPath::absolute(GUEST_WORKSPACE)?;
    tar_pipe_transfer_to(target, source_dir, &workspace, exclude_git)
}

/// Transfer a local directory to an arbitrary guest path via tar-pipe.
pub fn tar_pipe_transfer_to(
    target: &SshTarget,
    source_dir: &Path,
    guest_path: &GuestPath,
    exclude_git: bool,
) -> Result<()> {
    tracing::info!(
        "Transferring {} to guest:{guest_path} via tar-pipe",
        source_dir.display(),
    );

    let mut tar_cmd = Command::new("tar");
    tar_cmd.args(["cf", "-"]);
    for exc in DEFAULT_EXCLUDES {
        tar_cmd.arg(format!("--exclude={exc}"));
    }
    if exclude_git {
        tar_cmd.arg(format!("--exclude={GIT_EXCLUDE}"));
    }
    // --exclude-vcs-ignores is GNU tar only (not available on macOS BSD tar)
    if !cfg!(target_os = "macos") {
        tar_cmd.arg("--exclude-vcs-ignores");
    }
    tar_cmd.arg("-C").arg(source_dir).arg(".");

    let mut tar_child = tar_cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start tar")?;

    let mut tar_stdout = tar_child
        .stdout
        .take()
        .context("Failed to get tar stdout")?;
    let tar_stderr = tar_child
        .stderr
        .take()
        .context("Failed to get tar stderr")?;

    let extract_cmd = tar_extract_cmd(guest_path);
    let mut ssh_args = target.ssh_opts();
    ssh_args.push(target.addr());
    ssh_args.push(extract_cmd.into_string());

    let mut ssh_child = Command::new("ssh")
        .args(&ssh_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start SSH for tar-pipe transfer")?;

    let mut ssh_stdin = ssh_child.stdin.take().context("Failed to get SSH stdin")?;
    let ssh_stderr = ssh_child
        .stderr
        .take()
        .context("Failed to get SSH stderr")?;

    // Drain both stderr streams in background threads. Remote tar
    // emits warnings ("Cannot change ownership", future timestamps)
    // during extraction; local tar can emit "file changed as we read
    // it" / "permission denied" on a busy workspace. Either side
    // filling its ~64K pipe buffer would block the producer and
    // deadlock the pipeline.
    let ssh_err_thread = drain_to_vec(ssh_stderr, "SSH stderr");
    let tar_err_thread = drain_to_vec(tar_stderr, "local tar stderr");

    // Stream tar output to SSH stdin.
    //
    // IO errors here (typically EPIPE when the remote tar exits early —
    // e.g. /workspace not writable, disk full) are *captured* rather
    // than propagated immediately so we can drain SSH's stderr below
    // and surface the actual root-cause to the user.
    let mut buf = vec![0u8; 64 * 1024];
    let mut transfer_err: Option<anyhow::Error> = None;
    loop {
        let n = match tar_stdout.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                transfer_err = Some(anyhow::Error::new(e).context("Failed to read tar output"));
                break;
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = ssh_stdin.write_all(&buf[..n]) {
            transfer_err = Some(anyhow::Error::new(e).context("Failed to write to SSH stdin"));
            break;
        }
    }
    drop(ssh_stdin);
    // Drop the tar stdout reader so any further tar writes get SIGPIPE
    // and tar can exit promptly instead of blocking on a full pipe.
    drop(tar_stdout);

    let ssh_status = ssh_child.wait().context("Failed to wait for SSH")?;
    let tar_status = tar_child.wait().context("Failed to wait for tar")?;
    let ssh_stderr_buf = join_drainer(ssh_err_thread)?;
    let tar_stderr_buf = join_drainer(tar_err_thread)?;

    // If the streaming loop failed (typically EPIPE), surface the
    // remote stderr — that's where "No space left on device" and
    // similar root-cause errors appear.
    if let Some(err) = transfer_err {
        return Err(err.context(format_remote_failure(&ssh_stderr_buf)));
    }

    if !tar_status.success() {
        bail!(
            "Local tar archive creation failed.\n{}",
            format_failure("Local tar stderr", &tar_stderr_buf, None)
        );
    }

    if !ssh_status.success() {
        bail!(
            "tar-pipe transfer to guest failed: {}",
            format_remote_failure(&ssh_stderr_buf)
        );
    }

    tracing::info!("Workspace transferred to guest");
    Ok(())
}

/// Spawn a background thread that reads `r` to EOF and returns the bytes.
///
/// Used to drain a child's stderr (or stdout) concurrently with the
/// main IO loop, avoiding deadlocks when the child writes more than
/// the pipe buffer can hold before the main thread is ready to read.
fn drain_to_vec<R: std::io::Read + Send + 'static>(
    mut r: R,
    label: &'static str,
) -> std::thread::JoinHandle<Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        r.read_to_end(&mut buf)
            .with_context(|| format!("Failed to drain {label}"))?;
        Ok(buf)
    })
}

fn join_drainer(handle: std::thread::JoinHandle<Result<Vec<u8>>>) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("stderr drainer thread panicked"))?
}

const GUEST_DISK_HINT: &str = "The guest disk is likely too small for this \
    workspace. Recreate it with a larger disk: `coop up --disk <GiB> ...`.";

/// Build a user-facing message from a captured stderr buffer.
///
/// `label` prefixes the body ("Remote stderr", "Local tar stderr", …).
/// When `disk_hint` is supplied, an empty buffer or ENOSPC/quota-style
/// content triggers the hint — ENOSPC is the most common silent
/// failure mode for tar pipelines and the underlying error message is
/// often unhelpful on its own.
fn format_failure(label: &str, stderr: &[u8], disk_hint: Option<&str>) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        return match disk_hint {
            Some(hint) => format!("{label} empty (command exited with no diagnostic). {hint}"),
            None => format!("{label} empty (command exited with no diagnostic)."),
        };
    }
    let lower = stderr.to_ascii_lowercase();
    let is_enospc =
        lower.contains("no space left on device") || lower.contains("disk quota exceeded");
    match disk_hint.filter(|_| is_enospc) {
        Some(hint) => format!("{label}:\n{stderr}\n\n{hint}"),
        None => format!("{label}:\n{stderr}"),
    }
}

/// Format the SSH-side stderr for a tar-pipe transfer failure.
///
/// Adds the guest-disk hint because the most common silent failure
/// of `tar_pipe_transfer` is the guest filesystem running out of
/// space mid-extraction.
fn format_remote_failure(stderr: &[u8]) -> String {
    format_failure("Remote stderr", stderr, Some(GUEST_DISK_HINT))
}

/// Combine SSH and local-tar stderr into a single user-facing block.
///
/// Used by `tar_pipe_pull`'s error paths so a cascade (remote tar
/// dies → SSH closes stdout truncated → local tar errors on the
/// truncated archive) reports both root cause and downstream effect.
/// Either side may be empty; in that case we emit only what we have.
fn format_pull_failure(ssh_stderr: &[u8], tar_stderr: &[u8]) -> String {
    let ssh_trim = String::from_utf8_lossy(ssh_stderr);
    let tar_trim = String::from_utf8_lossy(tar_stderr);
    match (ssh_trim.trim().is_empty(), tar_trim.trim().is_empty()) {
        (true, true) => "No diagnostic output from either side.".to_string(),
        (false, true) => format_failure("Remote stderr", ssh_stderr, None),
        (true, false) => format_failure("Local tar stderr", tar_stderr, None),
        (false, false) => format!(
            "{}\n{}",
            format_failure("Remote stderr", ssh_stderr, None),
            format_failure("Local tar stderr", tar_stderr, None),
        ),
    }
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

/// Reject an extra mount that would collide with the `/workspace` path a
/// `coop up --git-repo` clone occupies.
pub(crate) fn validate_git_repo_workspace_mounts(mounts: &[Mount]) -> Result<()> {
    let workspace_path = default_workspace_path().to_string();
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

/// Push local directory to guest. Uses rsync if available, falls back to tar-pipe.
///
/// Takes a `RunningInstance` so the caller's proof of liveness is
/// visible in the signature — no surprise SSH failure inside.
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

    if target.exec_ok(RemoteCommand::new().literal("which rsync")) {
        rsync_push(target, &source_dir, &state.guest_path, exclude_git)?;
    } else {
        tracing::info!("rsync not available on guest, using tar-pipe");
        tar_pipe_transfer(target, &source_dir, exclude_git)?;
    }

    tracing::info!("Push complete");
    Ok(())
}

/// Pull guest workspace to local directory. Uses rsync if available, falls back to tar-pipe.
///
/// Takes a `RunningInstance` so the caller's proof of liveness is
/// visible in the signature — no surprise SSH failure inside.
pub fn pull(
    running: &RunningInstance,
    dir: Option<&str>,
    force: bool,
    exclude_git: bool,
) -> Result<()> {
    let inst = running.instance();
    let target = running.target();
    let state = load_or_default(inst, dir, "pull")?;
    let dest_dir = resolve_host_dir(dir, &state, "pull")?;

    if !force && dest_dir.exists() {
        check_local_dirty(&dest_dir)?;
    }

    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("Failed to create {}", dest_dir.display()))?;

    tracing::info!(
        "Pulling guest:{} -> {}",
        state.guest_path,
        dest_dir.display()
    );

    if target.exec_ok(RemoteCommand::new().literal("which rsync")) {
        rsync_pull(target, &state.guest_path, &dest_dir, exclude_git)?;
    } else {
        tracing::info!("rsync not available on guest, using tar-pipe");
        tar_pipe_pull(target, &state.guest_path, &dest_dir, exclude_git)?;
    }

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

        if target.exec_ok(RemoteCommand::new().literal("which rsync")) {
            rsync_push(target, &m.host_path, guest, exclude_git)?;
        } else {
            tracing::info!("rsync not available on guest, using tar-pipe");
            tar_pipe_transfer_to(target, &m.host_path, guest, exclude_git)?;
        }
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

/// Generate SSH config and launch VS Code Remote SSH.
///
/// Takes a `RunningInstance` so the caller's proof of liveness is
/// visible in the signature — VS Code needs a live SSH target.
pub fn vscode(
    running: &RunningInstance,
    project: Option<&str>,
    editor: Option<&str>,
) -> Result<()> {
    let inst = running.instance();
    let remote_path = match project {
        Some(p) => GuestPath::absolute(p)
            .with_context(|| format!("--project must be an absolute guest path: {p:?}"))?,
        None => default_workspace_path(),
    };

    write_ssh_config(running)?;
    launch_editor(inst, &remote_path, editor)?;
    Ok(())
}

/// Install (or refresh) the `coop-<name>` SSH alias and print it.
///
/// Writes the `Host coop-<name>` block to `~/.ssh/config` idempotently,
/// then prints the block plus `ssh`/`scp`/`rsync` usage hints to stderr.
/// Shared by `coop vscode` (which also launches an editor) and the
/// standalone `coop ssh-config` command.
///
/// Takes a `RunningInstance` so the SSH target is proven live by the
/// type — a stale alias is worse than none.
pub fn write_ssh_config(running: &RunningInstance) -> Result<()> {
    let inst = running.instance();
    let target = running.target();

    update_ssh_config(target, inst)?;

    let host = ssh_config_host(inst);
    let block = ssh_config_block(target, inst);
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

fn rsync_base_args(target: &SshTarget, exclude_git: bool) -> Vec<String> {
    let mut args = vec!["-az".to_string(), "-e".to_string(), target.rsync_ssh_cmd()];
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
    args
}

pub(crate) fn rsync_push(
    target: &SshTarget,
    source: &Path,
    guest_path: &GuestPath,
    exclude_git: bool,
) -> Result<()> {
    let mut args = rsync_base_args(target, exclude_git);
    args.push("--delete".to_string());
    args.push(format!("{}/", source.display()));
    args.push(format!("{}:{guest_path}/", target.addr()));

    let status = Command::new("rsync")
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
    exclude_git: bool,
) -> Result<()> {
    let mut args = rsync_base_args(target, exclude_git);
    args.push(format!("{}:{guest_path}/", target.addr()));
    args.push(format!("{}/", dest.display()));

    let status = Command::new("rsync")
        .args(&args)
        .status()
        .context("Failed to run rsync")?;

    if !status.success() {
        bail!("rsync pull failed");
    }
    Ok(())
}

// ── Transport: tar-pipe ───────────────────────────────────────

/// Remote command that extracts a streamed tar archive into `guest_path`.
///
/// `guest_path` is shell-escaped (via [`RemoteCommand::arg`]) so it is treated
/// as a single, literal directory argument regardless of its contents.
fn tar_extract_cmd(guest_path: &GuestPath) -> RemoteCommand {
    RemoteCommand::new().literal("tar xf - -C ").arg(guest_path)
}

/// Remote command that streams a tar archive of `guest_path` to stdout,
/// applying the same exclusions as a push.
///
/// `guest_path` is shell-escaped (via [`RemoteCommand::arg`]); the exclude
/// patterns are fixed constants, not user input.
fn tar_pull_cmd(guest_path: &GuestPath, exclude_git: bool) -> RemoteCommand {
    let mut excludes: Vec<String> = DEFAULT_EXCLUDES
        .iter()
        .map(|exc| format!("--exclude={exc}"))
        .collect();
    if exclude_git {
        excludes.push(format!("--exclude={GIT_EXCLUDE}"));
    }
    let exclude_str = excludes.join(" ");
    RemoteCommand::new()
        .literal("tar cf - -C ")
        .arg(guest_path)
        .literal(format!(" {exclude_str} ."))
}

fn tar_pipe_pull(
    target: &SshTarget,
    guest_path: &GuestPath,
    dest: &Path,
    exclude_git: bool,
) -> Result<()> {
    let remote_cmd = tar_pull_cmd(guest_path, exclude_git).into_string();

    let mut ssh_args = target.ssh_opts();
    ssh_args.push(target.addr());
    ssh_args.push(remote_cmd);

    let mut ssh_child = Command::new("ssh")
        .args(&ssh_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start SSH for tar-pipe pull")?;

    let mut ssh_stdout = ssh_child
        .stdout
        .take()
        .context("Failed to get SSH stdout")?;
    let ssh_stderr = ssh_child
        .stderr
        .take()
        .context("Failed to get SSH stderr")?;

    let mut tar_child = Command::new("tar")
        .arg("xf")
        .arg("-")
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start local tar extraction")?;

    let mut tar_stdin = tar_child.stdin.take().context("Failed to get tar stdin")?;
    let tar_stderr = tar_child
        .stderr
        .take()
        .context("Failed to get tar stderr")?;

    // Drain both stderr streams in background threads. Either side
    // can emit warnings during streaming and stall the pipeline if
    // its 64K pipe buffer fills before someone reads.
    let ssh_err_thread = drain_to_vec(ssh_stderr, "SSH stderr");
    let tar_err_thread = drain_to_vec(tar_stderr, "local tar stderr");

    // Stream SSH stdout to tar stdin. Capture IO errors instead of
    // propagating immediately so we can drain stderr below.
    let mut buf = vec![0u8; 64 * 1024];
    let mut transfer_err: Option<anyhow::Error> = None;
    loop {
        let n = match ssh_stdout.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                transfer_err = Some(anyhow::Error::new(e).context("Failed to read SSH output"));
                break;
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = tar_stdin.write_all(&buf[..n]) {
            transfer_err = Some(anyhow::Error::new(e).context("Failed to write to tar stdin"));
            break;
        }
    }
    drop(tar_stdin);
    drop(ssh_stdout);

    let ssh_status = ssh_child.wait().context("Failed to wait for SSH")?;
    let tar_status = tar_child.wait().context("Failed to wait for tar")?;
    let ssh_stderr_buf = join_drainer(ssh_err_thread)?;
    let tar_stderr_buf = join_drainer(tar_err_thread)?;

    if let Some(err) = transfer_err {
        return Err(err.context(format_pull_failure(&ssh_stderr_buf, &tar_stderr_buf)));
    }

    // Check SSH first: if the remote tar dies mid-archive, SSH closes
    // stdout truncated and local tar then errors on the malformed
    // record. Reporting tar's "Unexpected EOF" would hide the actual
    // root cause in ssh_stderr_buf.
    if !ssh_status.success() {
        bail!(
            "tar-pipe pull from guest failed.\n{}",
            format_pull_failure(&ssh_stderr_buf, &tar_stderr_buf)
        );
    }

    if !tar_status.success() {
        bail!(
            "tar extraction failed during pull.\n{}",
            format_failure("Local tar stderr", &tar_stderr_buf, None)
        );
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────

fn resolve_host_dir(explicit: Option<&str>, state: &WorkspaceState, cmd: &str) -> Result<PathBuf> {
    if let Some(d) = explicit {
        return Ok(PathBuf::from(d));
    }
    state
        .source
        .host_path()
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "No host_path in workspace.json and no --dir given.\n\
                 Provide a directory: coop {cmd} --dir ./my-project"
            )
        })
}

fn check_guest_dirty(target: &SshTarget, guest_path: &GuestPath) -> Result<()> {
    // Untracked files are excluded — they're almost always host-side noise
    // (build artifacts, editor state) that was copied in at start time, not
    // edits made inside the guest. Modified tracked files and unpushed
    // commits are the real signal that an agent has done work the host
    // doesn't yet know about.
    let check_cmd = RemoteCommand::new()
        .literal("if [ -d ")
        .arg(guest_path)
        .literal("/.git ]; then cd ")
        .arg(guest_path)
        .literal(
            " && \
             git status --porcelain --untracked-files=no && \
             if git rev-parse --abbrev-ref '@{u}' >/dev/null 2>&1; then \
                 ahead=$(git rev-list --count '@{u}..HEAD' 2>/dev/null); \
                 if [ \"${ahead:-0}\" -gt 0 ]; then \
                     echo \"AHEAD $ahead\"; \
                 fi; \
             fi; \
          fi",
        );

    let mut args = target.ssh_opts();
    args.push(target.addr());
    args.push(check_cmd.into_string());

    let output = Command::new("ssh")
        .args(&args)
        .output()
        .context("Failed to check guest workspace status")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        bail!(
            "Guest workspace has changes the host does not know about:\n{stdout}\n\
             Pull them with `coop pull`, or overwrite with `coop push --force`."
        );
    }

    Ok(())
}

fn check_local_dirty(dest: &Path) -> Result<()> {
    let git_dir = dest.join(".git");
    if !git_dir.exists() {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["status", "--porcelain"])
        .output()
        .context("Failed to check local git status")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        bail!(
            "Local directory has uncommitted changes:\n{stdout}\n\
             Use --force to overwrite"
        );
    }

    Ok(())
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

fn ssh_config_block(target: &SshTarget, inst: &Instance) -> String {
    let host = ssh_config_host(inst);
    format!(
        "{MARKER_PREFIX} {host}\n\
         Host {host}\n\
         \x20   HostName {}\n\
         \x20   Port {}\n\
         \x20   User {}\n\
         \x20   IdentityFile {}\n\
         \x20   IdentitiesOnly yes\n\
         \x20   StrictHostKeyChecking no\n\
         \x20   UserKnownHostsFile /dev/null\n\
         \x20   LogLevel ERROR\n\
         {MARKER_END}",
        target.host,
        target.port,
        target.user,
        target.key_path.display(),
    )
}

fn update_ssh_config(target: &SshTarget, inst: &Instance) -> Result<()> {
    let ssh_config = ssh_config_path()?;

    if let Some(parent) = ssh_config.parent() {
        fs::create_dir_all(parent).context("Failed to create ~/.ssh directory")?;
    }

    let block = ssh_config_block(target, inst);
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

struct LaunchStrategy {
    name: &'static str,
    cmd: String,
    args: Vec<String>,
}

fn vscode_strategies(remote_arg: &str, remote_path: &GuestPath) -> Vec<LaunchStrategy> {
    let path_arg = remote_path.to_string();
    let mut strategies = vec![LaunchStrategy {
        name: "code CLI",
        cmd: "code".into(),
        args: vec!["--remote".into(), remote_arg.into(), path_arg.clone()],
    }];
    if cfg!(target_os = "macos") {
        strategies.push(LaunchStrategy {
            name: "macOS open -a 'Visual Studio Code'",
            cmd: "open".into(),
            args: vec![
                "-a".into(),
                "Visual Studio Code".into(),
                "--args".into(),
                "--remote".into(),
                remote_arg.into(),
                path_arg,
            ],
        });
    }
    strategies
}

fn launch_editor(inst: &Instance, remote_path: &GuestPath, editor: Option<&str>) -> Result<()> {
    let host = ssh_config_host(inst);
    let remote_arg = format!("ssh-remote+{host}");

    let strategies = match editor {
        None | Some("code") => vscode_strategies(&remote_arg, remote_path),
        Some(other) => bail!("Unknown editor '{other}'. Supported: code"),
    };

    let mut tried = Vec::new();
    for strategy in &strategies {
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
                tried.push(strategy.name);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!("{}: command not found", strategy.name);
                tried.push(strategy.name);
            }
            Err(e) => {
                return Err(anyhow::anyhow!("{} failed: {e}", strategy.name));
            }
        }
    }

    bail!(
        "Could not open VS Code. Tried:\n{}\n\n\
         To install the `code` CLI: open VS Code, \
         Cmd+Shift+P, 'Shell Command: Install'",
        tried
            .iter()
            .map(|n| format!("  - {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::config::{ImageName, InstanceIndex, InstanceName};
    use proptest::prelude::*;

    fn temp_instance(dir: &Path) -> Instance {
        Instance {
            name: InstanceName::new("test").expect("valid name"),
            index: InstanceIndex::new(0).expect("0 is in range"),
            dir: dir.to_path_buf(),
            image: ImageName::new("default").expect("valid image name"),
        }
    }

    #[test]
    fn vscode_strategies_first_is_code_cli() {
        let path = GuestPath::new("/workspace");
        let strategies = vscode_strategies("ssh-remote+coop-test", &path);
        // The `code` CLI strategy is present on every platform (the macOS
        // `open -a` fallback is appended after it).
        let code = &strategies[0];
        assert_eq!(code.name, "code CLI");
        assert_eq!(code.cmd, "code");
        let args: Vec<&str> = code.args.iter().map(String::as_str).collect();
        assert_eq!(args, ["--remote", "ssh-remote+coop-test", "/workspace"]);
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
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).expect("data");
        let mounts = vec![Mount::parse(data.to_str().unwrap()).expect("mount")];

        let err =
            validate_git_repo_workspace_mounts(&mounts).expect_err("expected /workspace collision");
        assert!(format!("{err}").contains("/workspace"));
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
    fn format_remote_failure_empty_stderr_hints_disk() {
        let msg = format_remote_failure(b"");
        assert!(msg.contains("disk"), "should hint at disk full: {msg}");
        assert!(msg.contains("--disk"), "should suggest --disk flag: {msg}");
    }

    #[test]
    fn format_remote_failure_passes_through_stderr() {
        let stderr = b"cat: write error: No space left on device\n";
        let msg = format_remote_failure(stderr);
        assert!(
            msg.contains("No space left on device"),
            "should surface remote stderr: {msg}"
        );
        assert!(
            msg.starts_with("Remote stderr:"),
            "should prefix with label: {msg}"
        );
    }

    #[test]
    fn format_remote_failure_trims_whitespace() {
        let msg = format_remote_failure(b"   \n\nactual error\n\n");
        assert!(msg.contains("actual error"));
        assert!(!msg.ends_with('\n'));
    }

    #[test]
    fn format_remote_failure_disk_quota_triggers_hint() {
        let msg = format_remote_failure(b"tar: write error: Disk quota exceeded\n");
        assert!(msg.contains("Disk quota exceeded"));
        assert!(
            msg.contains("--disk"),
            "quota should also trigger hint: {msg}"
        );
    }

    #[test]
    fn format_failure_without_hint_omits_hint_when_empty() {
        let msg = format_failure("Local tar stderr", b"", None);
        assert!(msg.starts_with("Local tar stderr empty"));
        assert!(!msg.contains("--disk"));
    }

    #[test]
    fn format_failure_without_hint_skips_enospc_hint() {
        let msg = format_failure("Local tar stderr", b"No space left on device", None);
        assert!(msg.contains("No space left on device"));
        assert!(
            !msg.contains("--disk"),
            "no hint configured, must not invent one: {msg}"
        );
    }

    #[test]
    fn format_pull_failure_combines_both_when_present() {
        let msg = format_pull_failure(b"remote boom", b"local boom");
        assert!(msg.contains("Remote stderr:"));
        assert!(msg.contains("remote boom"));
        assert!(msg.contains("Local tar stderr:"));
        assert!(msg.contains("local boom"));
    }

    #[test]
    fn format_pull_failure_omits_empty_side() {
        let msg = format_pull_failure(b"remote boom", b"");
        assert!(msg.contains("Remote stderr:"));
        assert!(
            !msg.contains("Local tar stderr"),
            "should not mention empty side: {msg}"
        );
    }

    #[test]
    fn format_pull_failure_handles_both_empty() {
        let msg = format_pull_failure(b"", b"   \n");
        assert!(msg.contains("No diagnostic output"));
    }

    #[test]
    fn try_load_returns_none_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let result = WorkspaceState::try_load(&inst).expect("no IO error");
        assert!(result.is_none());
    }

    #[test]
    fn try_load_errors_on_invalid_json() {
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let primary = tempfile::tempdir().expect("primary");
        let secondary = tempfile::tempdir().expect("secondary");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
            let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let block = ssh_config_block(&fake_ssh_target(), &inst);

        assert!(block.starts_with("# coop START coop-test"));
        assert!(block.trim_end().ends_with("# coop END"));
        assert!(block.contains("Host coop-test"));
        assert!(block.contains("HostName 127.0.0.1"));
        assert!(block.contains("Port 2222"));
        assert!(block.contains("User ubuntu"));
        assert!(block.contains("IdentityFile /tmp/key"));
        assert!(block.contains("StrictHostKeyChecking no"));
        assert!(block.contains("UserKnownHostsFile /dev/null"));
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        SshTarget {
            host: Hostname::new("127.0.0.1").expect("valid host"),
            port: std::num::NonZeroU16::new(2222).unwrap(),
            user: SshUser::new("ubuntu").expect("valid user"),
            key_path: PathBuf::from("/tmp/key"),
        }
    }

    #[test]
    fn default_excludes_omit_git() {
        // Issue #91: `.git/` was previously hardcoded into DEFAULT_EXCLUDES,
        // which silently stripped git history from agents in the guest. The
        // policy is now opt-out via --exclude-git; lock it in here so a
        // future tidy-up doesn't accidentally re-add `.git/`.
        assert!(
            !DEFAULT_EXCLUDES.iter().any(|e| e.contains(".git")),
            "DEFAULT_EXCLUDES must not contain a .git pattern; got {DEFAULT_EXCLUDES:?}"
        );
        assert_eq!(GIT_EXCLUDE, ".git/");
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
            let args = rsync_base_args(&fake_ssh_target(), exclude_git);

            for exc in DEFAULT_EXCLUDES {
                let needle = format!("--exclude={exc}");
                prop_assert!(
                    args.iter().any(|a| a == &needle),
                    "missing default exclude {needle:?}: {args:?}"
                );
            }

            let has_protect = args.iter().any(|a| a == "--filter=+ /.git/***");
            let has_exclude_git = args.iter().any(|a| a == "--exclude=.git/");
            // Exactly one git rule, selected by the flag.
            prop_assert_eq!(has_protect, !exclude_git);
            prop_assert_eq!(has_exclude_git, exclude_git);

            let gitignore_idx = args
                .iter()
                .position(|a| a == "--filter=:- .gitignore")
                .expect("expected .gitignore merge filter");
            let git_rule_idx = args
                .iter()
                .position(|a| a == "--filter=+ /.git/***" || a == "--exclude=.git/")
                .expect("expected a git rule");
            prop_assert!(
                git_rule_idx < gitignore_idx,
                "git rule must precede the .gitignore merge: {:?}",
                args
            );
        }
    }

    // ── WorkspaceState serialization ──────────────────────────

    #[test]
    fn try_load_rejects_relative_guest_path() {
        // A hand-edited or pre-migration workspace.json with a relative
        // guest_path must be rejected at load — `GuestPath`'s transparent
        // deserialize would otherwise accept it and let it flow into the
        // remote shell commands that interpolate the path.
        let dir = tempfile::tempdir().expect("tempdir");
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
    fn tar_remote_cmds_quote_guest_path_metacharacters() {
        // An absolute path still satisfies `GuestPath::absolute` while
        // carrying shell metacharacters, so the remote tar commands must
        // single-quote it. The injected `;` and `$()` must appear only
        // inside the quoted argument, never as live shell syntax.
        let evil = GuestPath::absolute("/work; rm -rf / $(touch pwned)").expect("absolute");

        let extract = tar_extract_cmd(&evil).into_string();
        assert_eq!(
            extract, "tar xf - -C '/work; rm -rf / $(touch pwned)'",
            "extract command must single-quote the guest path"
        );

        let pull = tar_pull_cmd(&evil, false).into_string();
        assert!(
            pull.starts_with("tar cf - -C '/work; rm -rf / $(touch pwned)' "),
            "pull command must single-quote the guest path: {pull}"
        );
    }

    #[test]
    fn try_load_legacy_shape_emits_migration_hint() {
        // The pre-#147 flat shape no longer parses; the error context
        // points the user at the remediation.
        let dir = tempfile::tempdir().expect("tempdir");
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
