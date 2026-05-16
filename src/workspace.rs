use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::backend::SshTarget;
use crate::config::Instance;

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
    /// Host directory path (None for git-repo clones with no local origin)
    pub host_path: Option<PathBuf>,
    /// Path inside the guest VM
    pub guest_path: String,
    /// How the workspace was created
    pub source: WorkspaceSource,
    /// Original repo URL when source is [`WorkspaceSource::GitRepo`].
    ///
    /// Recorded so that follow-up commands (`coop shell`, `claude`, etc.)
    /// can re-derive the `owner/repo` slug without re-asking — pat-mode
    /// otherwise has no way to look up the entry for a git-repo instance
    /// where `host_path` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_repo_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSource {
    Workspace,
    GitRepo,
    Mount,
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
        match fs::read_to_string(&path) {
            Ok(content) => {
                let state =
                    serde_json::from_str(&content).context("Failed to parse workspace.json")?;
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!(e).context(format!("Failed to read {}", path.display()))),
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
    tracing::info!(
        "Transferring {} to guest:{GUEST_WORKSPACE} via tar-pipe",
        source_dir.display()
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

    let extract_cmd = format!("tar xf - -C {GUEST_WORKSPACE}");
    let mut ssh_args = target.ssh_opts();
    ssh_args.push(target.addr());
    ssh_args.push(extract_cmd);

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
    workspace. Retry with a larger disk: `coop start --disk <GiB> ...`.";

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

fn load_or_default(inst: &Instance, dir: Option<&str>, cmd: &str) -> Result<WorkspaceState> {
    if let Some(state) = WorkspaceState::try_load(inst)? {
        return Ok(state);
    }
    if dir.is_some() {
        return Ok(WorkspaceState {
            host_path: None,
            guest_path: GUEST_WORKSPACE.to_string(),
            source: WorkspaceSource::Workspace,
            git_repo_url: None,
        });
    }
    bail!(
        "No workspace.json found and no --dir given.\n\
         Either start the VM with --workspace or provide a path: \
         coop {cmd} --dir ./my-project"
    )
}

/// Push local directory to guest. Uses rsync if available, falls back to tar-pipe.
pub fn push(
    target: &SshTarget,
    inst: &Instance,
    dir: Option<&str>,
    force: bool,
    exclude_git: bool,
) -> Result<()> {
    let state = load_or_default(inst, dir, "push")?;
    let source_dir = resolve_host_dir(dir, &state)?;

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

    if target.exec_ok("which rsync") {
        rsync_push(target, &source_dir, &state.guest_path, exclude_git)?;
    } else {
        tracing::info!("rsync not available on guest, using tar-pipe");
        tar_pipe_transfer(target, &source_dir, exclude_git)?;
    }

    tracing::info!("Push complete");
    Ok(())
}

/// Pull guest workspace to local directory. Uses rsync if available, falls back to tar-pipe.
pub fn pull(
    target: &SshTarget,
    inst: &Instance,
    dir: Option<&str>,
    force: bool,
    exclude_git: bool,
) -> Result<()> {
    let state = load_or_default(inst, dir, "pull")?;
    let dest_dir = resolve_host_dir_for_pull(dir, &state)?;

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

    if target.exec_ok("which rsync") {
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
    for m in mounts {
        let guest = &m.guest_path;
        target.exec(&format!(
            "sudo mkdir -p {guest} && sudo chown ubuntu:ubuntu {guest}"
        ))?;

        tracing::info!("Syncing {} -> guest:{guest}", m.host_path.display(),);

        if target.exec_ok("which rsync") {
            rsync_push(target, &m.host_path, guest, exclude_git)?;
        } else {
            tracing::info!("rsync not available on guest, using tar-pipe");
            tar_pipe_transfer(target, &m.host_path, exclude_git)?;
        }
    }

    // Save state for the first mount so push/pull work
    if let Some(m) = mounts.first() {
        let state = WorkspaceState {
            host_path: Some(m.host_path.clone()),
            guest_path: m.guest_path.clone(),
            source: WorkspaceSource::Mount,
            git_repo_url: None,
        };
        state.save(inst)?;
    }

    Ok(())
}

/// Generate SSH config and launch VS Code Remote SSH.
pub fn vscode(
    target: &SshTarget,
    inst: &Instance,
    project: Option<&str>,
    editor: Option<&str>,
) -> Result<()> {
    let remote_path = project.unwrap_or(GUEST_WORKSPACE);

    update_ssh_config(target, inst)?;
    launch_editor(inst, remote_path, editor)?;

    let block = ssh_config_block(target, inst);
    writeln!(
        std::io::stderr(),
        "\nSSH config entry (for manual editor connections):\n\n{block}"
    )
    .context("Failed to write SSH config info")?;

    Ok(())
}

/// Remove SSH config blocks for all instances from ~/.ssh/config.
pub fn remove_all_ssh_config() -> Result<()> {
    let ssh_config = ssh_config_path()?;
    if !ssh_config.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&ssh_config).context("Failed to read ~/.ssh/config")?;

    let cleaned = remove_marker_blocks(&content);
    if cleaned.len() != content.len() {
        atomic_write(&ssh_config, &cleaned).context("Failed to write ~/.ssh/config")?;
        tracing::info!("Removed coop SSH config blocks");
    }

    Ok(())
}

/// Remove SSH config block for a specific instance.
pub fn remove_ssh_config(inst: &Instance) -> Result<()> {
    let ssh_config = ssh_config_path()?;
    if !ssh_config.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&ssh_config).context("Failed to read ~/.ssh/config")?;
    let host_name = ssh_config_host(inst);

    let cleaned = remove_named_marker_block(&content, &host_name);
    if cleaned.len() != content.len() {
        atomic_write(&ssh_config, &cleaned).context("Failed to write ~/.ssh/config")?;
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

pub fn rsync_push(
    target: &SshTarget,
    source: &Path,
    guest_path: &str,
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

fn rsync_pull(target: &SshTarget, guest_path: &str, dest: &Path, exclude_git: bool) -> Result<()> {
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

fn tar_pipe_pull(
    target: &SshTarget,
    guest_path: &str,
    dest: &Path,
    exclude_git: bool,
) -> Result<()> {
    let mut excludes: Vec<String> = DEFAULT_EXCLUDES
        .iter()
        .map(|exc| format!("--exclude={exc}"))
        .collect();
    if exclude_git {
        excludes.push(format!("--exclude={GIT_EXCLUDE}"));
    }
    let exclude_str = excludes.join(" ");

    let remote_cmd = format!("tar cf - -C {guest_path} {exclude_str} .");

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

fn resolve_host_dir(explicit: Option<&str>, state: &WorkspaceState) -> Result<PathBuf> {
    if let Some(d) = explicit {
        return Ok(PathBuf::from(d));
    }
    state.host_path.clone().context(
        "No host_path in workspace.json and no --dir given.\n\
         Provide a directory: coop push --dir ./my-project",
    )
}

fn resolve_host_dir_for_pull(explicit: Option<&str>, state: &WorkspaceState) -> Result<PathBuf> {
    if let Some(d) = explicit {
        return Ok(PathBuf::from(d));
    }
    if let Some(ref hp) = state.host_path {
        return Ok(hp.clone());
    }
    bail!(
        "No host_path in workspace.json and no --dir given.\n\
         Provide a destination: coop pull --dir ./my-project"
    )
}

fn check_guest_dirty(target: &SshTarget, guest_path: &str) -> Result<()> {
    // Untracked files are excluded — they're almost always host-side noise
    // (build artifacts, editor state) that was copied in at start time, not
    // edits made inside the guest. Modified tracked files and unpushed
    // commits are the real signal that an agent has done work the host
    // doesn't yet know about.
    let check_cmd = format!(
        "if [ -d {guest_path}/.git ]; then \
            cd {guest_path} && \
            git status --porcelain --untracked-files=no && \
            if git rev-parse --abbrev-ref '@{{u}}' >/dev/null 2>&1; then \
                ahead=$(git rev-list --count '@{{u}}..HEAD' 2>/dev/null); \
                if [ \"${{ahead:-0}}\" -gt 0 ]; then \
                    echo \"AHEAD $ahead\"; \
                fi; \
            fi; \
         fi"
    );

    let mut args = target.ssh_opts();
    args.push(target.addr());
    args.push(check_cmd);

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

fn vscode_strategies(remote_arg: &str, remote_path: &str) -> Vec<LaunchStrategy> {
    let mut strategies = vec![LaunchStrategy {
        name: "code CLI",
        cmd: "code".into(),
        args: vec!["--remote".into(), remote_arg.into(), remote_path.into()],
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
                remote_path.into(),
            ],
        });
    }
    strategies
}

fn launch_editor(inst: &Instance, remote_path: &str, editor: Option<&str>) -> Result<()> {
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
    use crate::config::{InstanceIndex, InstanceName};

    fn temp_instance(dir: &Path) -> Instance {
        Instance {
            name: InstanceName::new("test").expect("valid name"),
            index: InstanceIndex::new(0),
            dir: dir.to_path_buf(),
            image: "default".to_string(),
        }
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
    fn try_load_returns_state_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let state = WorkspaceState {
            host_path: Some(PathBuf::from("/tmp/project")),
            guest_path: "/workspace".to_string(),
            source: WorkspaceSource::Workspace,
            git_repo_url: None,
        };
        state.save(&inst).expect("save");
        let loaded = WorkspaceState::try_load(&inst)
            .expect("no IO error")
            .expect("should be Some");
        assert_eq!(loaded.guest_path, "/workspace");
        assert_eq!(loaded.host_path.as_deref(), Some(Path::new("/tmp/project")));
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
    fn load_or_default_uses_saved_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        let state = WorkspaceState {
            host_path: Some(PathBuf::from("/host/dir")),
            guest_path: "/custom".to_string(),
            source: WorkspaceSource::GitRepo,
            git_repo_url: None,
        };
        state.save(&inst).expect("save");
        let loaded = load_or_default(&inst, None, "push").expect("load");
        assert_eq!(loaded.guest_path, "/custom");
    }

    #[test]
    fn load_or_default_falls_back_with_dir_arg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inst = temp_instance(dir.path());
        // No workspace.json exists
        let loaded = load_or_default(&inst, Some("./project"), "push").expect("load");
        assert_eq!(loaded.guest_path, GUEST_WORKSPACE);
        assert!(loaded.host_path.is_none());
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
    fn remove_all_from_only_moat_blocks_returns_empty() {
        let input = "\
# coop START coop-0\n\
Host coop-0\n\
    HostName 172.16.0.2\n\
# coop END\n";

        let result = remove_marker_blocks(input);
        assert!(result.is_empty());
    }

    // ── exclude_git policy ────────────────────────────────────

    fn fake_ssh_target() -> SshTarget {
        SshTarget {
            host: "127.0.0.1".to_string(),
            port: std::num::NonZeroU16::new(2222).unwrap(),
            user: "ubuntu".to_string(),
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

    #[test]
    fn rsync_args_include_git_by_default() {
        let args = rsync_base_args(&fake_ssh_target(), false);
        // The protective filter must precede the .gitignore merge so
        // first-match-wins doesn't let a user's .gitignore strip .git/.
        let protect_idx = args
            .iter()
            .position(|a| a == "--filter=+ /.git/***")
            .expect("expected protective .git/ include filter");
        let gitignore_idx = args
            .iter()
            .position(|a| a == "--filter=:- .gitignore")
            .expect("expected .gitignore merge filter");
        assert!(
            protect_idx < gitignore_idx,
            "protective filter must come before .gitignore merge: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--exclude=.git/"),
            "default rsync must not exclude .git/: {args:?}"
        );
    }

    #[test]
    fn rsync_args_exclude_git_when_requested() {
        let args = rsync_base_args(&fake_ssh_target(), true);
        // Opt-out path: no protective filter, explicit exclude before the
        // .gitignore merge so the exclude wins.
        let exclude_idx = args
            .iter()
            .position(|a| a == "--exclude=.git/")
            .expect("expected --exclude=.git/");
        let gitignore_idx = args
            .iter()
            .position(|a| a == "--filter=:- .gitignore")
            .expect("expected .gitignore merge filter");
        assert!(
            exclude_idx < gitignore_idx,
            "explicit --exclude=.git/ must precede .gitignore merge: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--filter=+ /.git/***"),
            "exclude_git=true must drop the protective filter: {args:?}"
        );
    }
}
