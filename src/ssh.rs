use std::io::Write as _;
use std::process::Command;

use anyhow::{Context, Result};

use crate::backend::SshSession;
use crate::shell::shell_escape;

/// Return a TERM value the guest is guaranteed to understand.
///
/// Modern terminals (Ghostty, Kitty, `WezTerm`) set custom TERM values
/// whose terminfo entries aren't in a stock Ubuntu install. SSH
/// forwards TERM automatically, so the guest gets a value it can't
/// resolve — causing "missing or unsuitable terminal" errors.
/// Fall back to `xterm-256color` which is universally available.
fn guest_term() -> String {
    let term = std::env::var("TERM").unwrap_or_default();
    let safe = ["xterm", "xterm-256color", "screen", "tmux", "vt100"];
    if safe.iter().any(|&s| term == s) {
        term
    } else {
        "xterm-256color".to_string()
    }
}

/// Open an interactive SSH session to the guest VM.
///
/// When `tmux_session` is `Some`, the session runs inside a named
/// tmux session that survives SSH disconnects. Reconnecting
/// reattaches to the existing session.
pub fn connect(session: &SshSession, tmux_session: Option<&str>) -> Result<()> {
    tracing::info!(
        "Connecting via SSH to {}:{}",
        session.target.host,
        session.target.port
    );

    let mut args = session.ssh_opts();
    args.push(session.target.addr());

    let remote_cmd = match tmux_session {
        Some(name) => format!(
            "tmux new-session -A -s {} -c /workspace",
            shell_escape(name),
        ),
        None => "cd /workspace && exec $SHELL -l".to_string(),
    };
    args.extend(["-t".to_string(), remote_cmd]);

    let status = Command::new("ssh")
        .args(&args)
        .envs(session.env.as_envs())
        .env("TERM", guest_term())
        .status()
        .context("Failed to launch SSH — is the ssh client installed?")?;

    if !status.success() {
        tracing::warn!("SSH session exited with status: {status}");
    }

    Ok(())
}

/// Run a command non-interactively over SSH (no PTY).
///
/// Propagates the remote command's exit code via the process exit code.
pub fn run_command(session: &SshSession, command: &[String]) -> Result<()> {
    let remote_cmd = command
        .iter()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ");

    tracing::info!("Running (non-interactive): {remote_cmd}");

    let mut args = session.ssh_opts();
    args.push(session.target.addr());
    args.push(remote_cmd);

    let status = Command::new("ssh")
        .args(&args)
        .envs(session.env.as_envs())
        .status()
        .context("Failed to launch SSH")?;

    if !status.success() {
        anyhow::bail!("Remote command exited with status: {status}");
    }

    Ok(())
}

/// Run a command interactively over SSH with a PTY.
///
/// When `tmux_session` is `Some`, the command runs inside a named
/// tmux session. If the session already exists, it reattaches
/// (the command argument is ignored by tmux on reattach).
pub fn run_interactive(
    session: &SshSession,
    cmd: &str,
    extra_args: &[String],
    tmux_session: Option<&str>,
) -> Result<()> {
    let mut inner_cmd = cmd.to_string();
    for arg in extra_args {
        inner_cmd.push(' ');
        inner_cmd.push_str(&shell_escape(arg));
    }

    let remote_cmd = match tmux_session {
        Some(name) => {
            format!(
                "tmux new-session -A -s {} -c /workspace {}",
                shell_escape(name),
                shell_escape(&inner_cmd),
            )
        }
        None => format!("cd /workspace && {inner_cmd}"),
    };

    tracing::info!("Running: {remote_cmd}");

    let mut args = session.ssh_opts();
    args.push(session.target.addr());
    args.extend(["-t".to_string(), remote_cmd]);

    let status = Command::new("ssh")
        .args(&args)
        .envs(session.env.as_envs())
        .env("TERM", guest_term())
        .status()
        .context("Failed to launch SSH")?;

    if !status.success() {
        tracing::warn!("SSH session exited with status: {status}");
    }

    Ok(())
}

/// Run a command in the VM, capture output, and exit with the remote's code.
///
/// Stdout and stderr from the remote command are written to the local
/// stdout/stderr respectively. The process exits with the remote
/// command's exit code, making this suitable for scripting and CI.
pub fn exec_command(session: &SshSession, command: &[String]) -> Result<()> {
    let remote_cmd = command
        .iter()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ");

    tracing::debug!("exec: {remote_cmd}");

    let mut args = session.ssh_opts();
    args.push(session.target.addr());
    args.push(remote_cmd);

    let output = Command::new("ssh")
        .args(&args)
        .envs(session.env.as_envs())
        .output()
        .context("Failed to launch SSH")?;

    std::io::stdout()
        .write_all(&output.stdout)
        .context("Failed to write stdout")?;
    std::io::stderr()
        .write_all(&output.stderr)
        .context("Failed to write stderr")?;

    if !output.status.success() {
        let code = output.status.code().unwrap_or(1);
        anyhow::bail!("Remote command exited with status {code}");
    }

    Ok(())
}
