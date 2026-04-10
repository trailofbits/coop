use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::backend::SshTarget;
use crate::config::Instance;

const GUEST_WORKSPACE: &str = "/workspace";

/// Default exclusions for transfers.
const DEFAULT_EXCLUDES: &[&str] = &[
    ".git/",
    "node_modules/",
    "target/",
    "__pycache__/",
    ".venv/",
    ".coop/",
];

/// Persisted workspace metadata written during `start`.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Host directory path (None for git-repo clones with no local origin)
    pub host_path: Option<PathBuf>,
    /// Path inside the guest VM
    pub guest_path: String,
    /// How the workspace was created
    pub source: WorkspaceSource,
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
/// Streams the tar archive through a SHA-256 hasher locally while
/// the remote independently hashes and extracts. Checksums are
/// compared after transfer to detect corruption.
pub fn tar_pipe_transfer(target: &SshTarget, source_dir: &Path) -> Result<()> {
    tracing::info!(
        "Transferring {} to guest:{GUEST_WORKSPACE} via tar-pipe",
        source_dir.display()
    );

    let mut tar_cmd = Command::new("tar");
    tar_cmd.args(["cf", "-"]);
    for exc in DEFAULT_EXCLUDES {
        tar_cmd.arg(format!("--exclude={exc}"));
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

    // Remote: save stream, hash it, then extract
    let extract_cmd = format!(
        "t=$(mktemp) && cat >\"$t\" && \
         h=$(sha256sum \"$t\" | cut -d' ' -f1) && \
         tar xf \"$t\" -C {GUEST_WORKSPACE} && \
         echo \"$h\" && rm -f \"$t\""
    );
    let mut ssh_args = target.ssh_opts();
    ssh_args.push(target.addr());
    ssh_args.push(extract_cmd);

    let mut ssh_child = Command::new("ssh")
        .args(&ssh_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start SSH for tar-pipe transfer")?;

    let mut ssh_stdin = ssh_child.stdin.take().context("Failed to get SSH stdin")?;

    // Stream tar output through SHA-256 hasher to SSH stdin
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = tar_stdout
            .read(&mut buf)
            .context("Failed to read tar output")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        ssh_stdin
            .write_all(&buf[..n])
            .context("Failed to write to SSH stdin")?;
    }
    drop(ssh_stdin);

    let local_hash = hex::encode(hasher.finalize());

    let output = ssh_child
        .wait_with_output()
        .context("Failed to wait for SSH transfer")?;

    let tar_status = tar_child.wait().context("Failed to wait for tar")?;
    if !tar_status.success() {
        bail!("Local tar archive creation failed");
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tar-pipe transfer to guest failed: {stderr}");
    }

    let remote_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if local_hash != remote_hash {
        bail!(
            "Checksum mismatch after tar-pipe transfer\n  \
             local:  {local_hash}\n  \
             remote: {remote_hash}\n\
             The workspace may be corrupted — retry the transfer"
        );
    }

    tracing::debug!("Transfer checksum verified: {local_hash}");
    tracing::info!("Workspace transferred to guest");
    Ok(())
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
        });
    }
    bail!(
        "No workspace.json found and no directory argument given.\n\
         Either start the VM with --workspace or provide a path: \
         coop {cmd} ./my-project"
    )
}

/// Push local directory to guest. Uses rsync if available, falls back to tar-pipe.
pub fn push(target: &SshTarget, inst: &Instance, dir: Option<&str>, force: bool) -> Result<()> {
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
        rsync_push(target, &source_dir, &state.guest_path)?;
    } else {
        tracing::info!("rsync not available on guest, using tar-pipe");
        tar_pipe_transfer(target, &source_dir)?;
    }

    tracing::info!("Push complete");
    Ok(())
}

/// Pull guest workspace to local directory. Uses rsync if available, falls back to tar-pipe.
pub fn pull(target: &SshTarget, inst: &Instance, dir: Option<&str>, force: bool) -> Result<()> {
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
        rsync_pull(target, &state.guest_path, &dest_dir)?;
    } else {
        tracing::info!("rsync not available on guest, using tar-pipe");
        tar_pipe_pull(target, &state.guest_path, &dest_dir)?;
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
) -> Result<()> {
    for m in mounts {
        let guest = &m.guest_path;
        target.exec(&format!(
            "sudo mkdir -p {guest} && sudo chown ubuntu:ubuntu {guest}"
        ))?;

        tracing::info!("Syncing {} -> guest:{guest}", m.host_path.display(),);

        if target.exec_ok("which rsync") {
            rsync_push(target, &m.host_path, guest)?;
        } else {
            tracing::info!("rsync not available on guest, using tar-pipe");
            tar_pipe_transfer(target, &m.host_path)?;
        }
    }

    // Save state for the first mount so push/pull work
    if let Some(m) = mounts.first() {
        let state = WorkspaceState {
            host_path: Some(m.host_path.clone()),
            guest_path: m.guest_path.clone(),
            source: WorkspaceSource::Mount,
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

fn rsync_base_args(target: &SshTarget) -> Vec<String> {
    let mut args = vec![
        "-az".to_string(),
        "-e".to_string(),
        target.rsync_ssh_cmd(),
        "--filter=:- .gitignore".to_string(),
    ];
    for exc in DEFAULT_EXCLUDES {
        args.push(format!("--exclude={exc}"));
    }
    args
}

pub fn rsync_push(target: &SshTarget, source: &Path, guest_path: &str) -> Result<()> {
    let mut args = rsync_base_args(target);
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

fn rsync_pull(target: &SshTarget, guest_path: &str, dest: &Path) -> Result<()> {
    let mut args = rsync_base_args(target);
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

fn tar_pipe_pull(target: &SshTarget, guest_path: &str, dest: &Path) -> Result<()> {
    let excludes: Vec<String> = DEFAULT_EXCLUDES
        .iter()
        .map(|exc| format!("--exclude={exc}"))
        .collect();
    let exclude_str = excludes.join(" ");

    // Remote: tar to temp, hash to stderr, stream to stdout
    let remote_cmd = format!(
        "t=$(mktemp) && \
         tar cf - -C {guest_path} {exclude_str} . >\"$t\" && \
         sha256sum \"$t\" | cut -d' ' -f1 >&2 && \
         cat \"$t\" && rm -f \"$t\""
    );

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

    // Collect stderr in background (contains remote hash)
    let stderr_handle = std::thread::spawn(move || {
        let mut stderr = ssh_stderr;
        let mut buf = String::new();
        stderr
            .read_to_string(&mut buf)
            .context("Failed to read SSH stderr")?;
        Ok::<_, anyhow::Error>(buf)
    });

    let mut tar_child = Command::new("tar")
        .arg("xf")
        .arg("-")
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to start local tar extraction")?;

    let mut tar_stdin = tar_child.stdin.take().context("Failed to get tar stdin")?;

    // Stream SSH stdout through SHA-256 hasher to tar stdin
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = ssh_stdout
            .read(&mut buf)
            .context("Failed to read SSH output")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        tar_stdin
            .write_all(&buf[..n])
            .context("Failed to write to tar stdin")?;
    }
    drop(tar_stdin);

    let local_hash = hex::encode(hasher.finalize());

    let tar_status = tar_child.wait().context("Failed to wait for tar")?;
    if !tar_status.success() {
        bail!("tar extraction failed during pull");
    }

    let ssh_status = ssh_child.wait().context("Failed to wait for SSH")?;
    if !ssh_status.success() {
        bail!("tar-pipe pull from guest failed");
    }

    let remote_stderr = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))?
        .context("Failed to read remote checksum")?;
    let remote_hash = remote_stderr.trim().to_string();

    if local_hash != remote_hash {
        bail!(
            "Checksum mismatch after tar-pipe pull\n  \
             local:  {local_hash}\n  \
             remote: {remote_hash}\n\
             The workspace may be corrupted — retry the transfer"
        );
    }

    tracing::debug!("Pull checksum verified: {local_hash}");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────

fn resolve_host_dir(explicit: Option<&str>, state: &WorkspaceState) -> Result<PathBuf> {
    if let Some(d) = explicit {
        return Ok(PathBuf::from(d));
    }
    state.host_path.clone().context(
        "No host_path in workspace.json and no directory argument given.\n\
         Provide a directory: coop push ./my-project",
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
        "No host_path in workspace.json and no directory argument given.\n\
         Provide a destination: coop pull ./my-project"
    )
}

fn check_guest_dirty(target: &SshTarget, guest_path: &str) -> Result<()> {
    let check_cmd = format!(
        "if [ -d {guest_path}/.git ]; then \
            cd {guest_path} && git status --porcelain; \
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
            "Guest workspace has uncommitted changes:\n{stdout}\n\
             Use --force to overwrite"
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
}
