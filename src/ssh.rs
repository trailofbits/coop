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

/// Force a known OpenSSH escape character for emergency disconnects.
///
/// OpenSSH only recognizes the escape at the start of a line, so users
/// should type Enter, then `~.`. Setting it here keeps user SSH config from
/// disabling or changing the escape path for coop's interactive sessions.
fn escape_opts() -> [String; 2] {
    ["-e".into(), "~".into()]
}

fn interactive_ssh_command(session: &SshSession, remote_cmd: &str) -> Result<Command> {
    let mut options = escape_opts().to_vec();
    options.push("-t".to_string());
    session.command(&options, remote_cmd)
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

    let status = interactive_ssh_command(session, &remote_cmd)?
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

    let status = session
        .command(&[], &remote_cmd)?
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

    let output = session
        .command(&[], &remote_cmd)?
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

        // The `ServerAlive*` pair comes from `SshTarget::transport_opts`, which
        // every transport shares; this session must not add a second one, since
        // OpenSSH honors the first value of a repeated `-o`.
        assert_eq!(
            interactive_ssh_command(&session, "cd /workspace && 'claude' 'agents'")
                .expect("interactive SSH command")
                .get_args()
                .collect::<Vec<_>>(),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
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
                "-e",
                "~",
                "-t",
                "ubuntu@127.0.0.1",
                "cd /workspace && 'claude' 'agents'",
            ],
        );
    }

    #[test]
    fn all_launch_paths_keep_guest_environment_off_host() {
        use std::os::unix::fs::PermissionsExt as _;
        if std::env::var_os("COOP_FORWARDING_FIXTURE").is_some() {
            let mut env = crate::backend::EnvForward::default();
            env.set("PATH", "./project-bin");
            env.set("LD_LIBRARY_PATH", "./project-libs");
            env.set("SECRET", "guest-only-sentinel");
            let session = SshSession {
                target: crate::backend::SshTarget {
                    host: crate::backend::Hostname::new("127.0.0.1").expect("host"),
                    port: std::num::NonZeroU16::MIN,
                    user: crate::backend::SshUser::new("ubuntu").expect("user"),
                    key_path: "/unused-key".into(),
                },
                env,
            };
            let check = "test \"$PATH\" = ./project-bin && test \"$LD_LIBRARY_PATH\" = ./project-libs && test \"$SECRET\" = guest-only-sentinel";
            let args = ["/bin/sh".into(), "-c".into(), check.into()];
            session
                .exec(crate::remote_command::RemoteCommand::new().literal(check))
                .expect("session exec");
            run_command(&session, &args).expect("noninteractive");
            exec_command(&session, &args).expect("captured");
            run_interactive(&session, &args).expect("interactive");
            return;
        }
        let fixture = tempfile::tempdir().expect("fixture");
        let marker = fixture.path().join("launched");
        let ssh = fixture.path().join("ssh");
        std::fs::write(
            &ssh,
            r#"#!/bin/sh
set -eu
test "$PATH" = "$COOP_FORWARDING_FIXTURE"
test "${LD_LIBRARY_PATH-unset}" = unset
test "${SECRET-unset}" = unset
for arg do remote=$arg; done
# Map the guest workspace to this fixture without requiring /workspace on macOS.
remote=$(printf '%s' "$remote" | /usr/bin/sed 's@cd /workspace && @@g')
# Simulate sshd running the remote command, after checking host isolation.
SHELL=/bin/bash /bin/sh -c "$remote"
printf x >> "$COOP_FORWARDING_MARKER"
"#,
        )
        .expect("SSH fixture");
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).expect("executable");
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "ssh::tests::all_launch_paths_keep_guest_environment_off_host",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", fixture.path())
            .env("COOP_FORWARDING_FIXTURE", fixture.path())
            .env("COOP_FORWARDING_MARKER", &marker)
            .output()
            .expect("child test");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(marker).expect("all launches reached fixture"),
            b"xxxx"
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
