use std::io::{IsTerminal as _, Write as _};
use std::process::Command;

use anyhow::{Context, Result};

use crate::backend::SshSession;
use crate::shell::shell_escape;

fn join_escaped(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render the single string passed to `ssh ... <cmd>`.
///
/// Empty `command` opens a bare interactive shell at `/workspace`.
fn render_remote(command: &[String]) -> String {
    if command.is_empty() {
        "cd /workspace && exec $SHELL -l".to_string()
    } else {
        format!("cd /workspace && {}", join_escaped(command))
    }
}

/// Return a TERM value the guest is guaranteed to understand.
///
/// Modern terminals (Ghostty, Kitty, `WezTerm`) set custom TERM values
/// whose terminfo entries aren't in a stock Ubuntu install. SSH
/// forwards TERM automatically, so the guest gets a value it can't
/// resolve — causing "missing or unsuitable terminal" errors.
/// Fall back to `xterm-256color` which is universally available.
fn guest_term() -> String {
    let term = std::env::var("TERM").unwrap_or_default();
    let safe = ["xterm", "xterm-256color", "screen", "vt100"];
    if safe.iter().any(|&s| term == s) {
        term
    } else {
        "xterm-256color".to_string()
    }
}

/// Keepalive options for interactive sessions.
///
/// A paused/suspended VM or a dropped local connection leaves SSH
/// blocked on a dead socket indefinitely. With these, SSH sends a probe
/// every 30s and gives up after 3 unanswered probes — so an unreachable
/// session terminates within ~90s instead of hanging. Scoped to
/// interactive use; the short-lived non-interactive paths don't need it.
fn keepalive_opts() -> [String; 4] {
    [
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ]
}

/// Force a known OpenSSH escape character for emergency disconnects.
///
/// OpenSSH only recognizes the escape at the start of a line, so users
/// should type Enter, then `~.`. Setting it here keeps user SSH config from
/// disabling or changing the escape path for coop's interactive sessions.
fn escape_opts() -> [String; 2] {
    ["-e".into(), "~".into()]
}

fn interactive_ssh_args(session: &SshSession, remote_cmd: String) -> Vec<String> {
    let mut args = session.ssh_opts();
    args.extend(keepalive_opts());
    args.extend(escape_opts());
    args.extend(["-t".to_string(), session.target.addr(), remote_cmd]);
    args
}

/// Restore the local terminal after an SSH failure.
///
/// When SSH itself fails (exit 255) the remote TUI never restores the
/// terminal, leaving it in raw mode and the alternate screen. Emit the
/// escape sequences to exit the alt-screen, show the cursor, re-enable
/// line wrap, and reset attributes, then run `stty sane` to restore line
/// discipline (echo, canonical mode). Best-effort: errors are ignored,
/// and it is a no-op when stdout isn't a terminal so pipes stay clean.
fn restore_terminal() {
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return;
    }
    let _ = stdout.write_all(b"\x1b[?1049l\x1b[?25h\x1b[?7h\x1b[0m");
    let _ = stdout.flush();
    let _ = Command::new("stty").arg("sane").status();
}

/// Run a command interactively over SSH with a PTY.
///
/// Empty `command` opens a bare interactive shell. Otherwise the
/// arguments are shell-escaped and run inside the user's login shell.
pub fn run_interactive(session: &SshSession, command: &[String]) -> Result<()> {
    let remote_cmd = render_remote(command);

    tracing::info!(
        "Connecting via SSH to {}:{} ({remote_cmd})",
        session.target.host,
        session.target.port,
    );
    tracing::info!(
        "If the remote session stops responding, type Enter, then ~. to disconnect; run `stty sane` if your terminal remains broken.",
    );

    let args = interactive_ssh_args(session, remote_cmd);

    let status = Command::new("ssh")
        .args(&args)
        .envs(session.env.as_envs())
        .env("TERM", guest_term())
        .status()
        .context("Failed to launch SSH — is the ssh client installed?")?;

    if !status.success() {
        tracing::warn!("SSH session exited with status: {status}");
        restore_terminal();
    }

    Ok(())
}

/// Run a command non-interactively over SSH (no PTY).
///
/// Propagates the remote command's exit code via the process exit code.
pub fn run_command(session: &SshSession, command: &[String]) -> Result<()> {
    let remote_cmd = join_escaped(command);

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

/// Run a command in the VM, capture output, and exit with the remote's code.
///
/// Stdout and stderr from the remote command are written to the local
/// stdout/stderr respectively. The process exits with the remote
/// command's exit code, making this suitable for scripting and CI.
pub fn exec_command(session: &SshSession, command: &[String]) -> Result<()> {
    let remote_cmd = join_escaped(command);

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

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests construct known-valid SSH values")]
mod tests {
    use super::*;

    #[test]
    fn empty_command_renders_interactive_shell() {
        assert_eq!(render_remote(&[]), "cd /workspace && exec $SHELL -l");
    }

    #[test]
    fn command_renders_with_cd_and_escaping() {
        let cmd = vec!["echo".into(), "hi".into()];
        assert_eq!(render_remote(&cmd), "cd /workspace && 'echo' 'hi'");
    }

    #[test]
    fn keepalive_opts_set_interval_and_count() {
        assert_eq!(
            keepalive_opts(),
            [
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
            ],
        );
    }

    #[test]
    fn escape_opts_force_tilde_escape() {
        assert_eq!(escape_opts(), ["-e", "~"]);
    }

    #[test]
    fn interactive_args_force_escape_before_target() {
        let session = SshSession {
            target: crate::backend::SshTarget {
                host: crate::backend::Hostname::new("127.0.0.1")
                    .expect("test host should be valid"),
                port: std::num::NonZeroU16::MIN,
                user: crate::backend::SshUser::new("ubuntu").expect("test user should be valid"),
                key_path: "/tmp/coop-test-key".into(),
            },
            env: crate::backend::EnvForward::default(),
        };

        assert_eq!(
            interactive_ssh_args(&session, "cd /workspace && 'claude' 'agents'".into()),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "LogLevel=ERROR",
                "-i",
                "/tmp/coop-test-key",
                "-p",
                "1",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
                "-e",
                "~",
                "-t",
                "ubuntu@127.0.0.1",
                "cd /workspace && 'claude' 'agents'",
            ],
        );
    }

    #[test]
    fn interactive_keepalive_is_not_shadowed_by_a_shorter_base_option() {
        // OpenSSH honors the first value of a repeated `-o`, so a keepalive
        // added to `SshTarget::ssh_opts` would silently win over this one and
        // cut interactive sessions at that shorter deadline instead of ~90s.
        let session = SshSession {
            target: crate::backend::SshTarget {
                host: crate::backend::Hostname::new("127.0.0.1")
                    .expect("test host should be valid"),
                port: std::num::NonZeroU16::MIN,
                user: crate::backend::SshUser::new("ubuntu").expect("test user should be valid"),
                key_path: "/tmp/coop-test-key".into(),
            },
            env: crate::backend::EnvForward::default(),
        };

        let args = interactive_ssh_args(&session, "cd /workspace && exec $SHELL -l".into());
        let effective: Vec<&String> = args
            .iter()
            .filter(|arg| arg.starts_with("ServerAlive"))
            .collect();
        assert_eq!(
            effective,
            ["ServerAliveInterval=30", "ServerAliveCountMax=3"],
        );
    }

    #[test]
    fn command_with_special_chars_is_escaped() {
        let cmd = vec!["/usr/bin/foo".into(), "--bar".into()];
        assert_eq!(
            render_remote(&cmd),
            "cd /workspace && '/usr/bin/foo' '--bar'",
        );
    }
}
