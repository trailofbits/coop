use std::fmt;
use std::io::Write as _;
use std::process::Command;
use std::str::FromStr;

use anyhow::{Context, Result, bail};

use crate::backend::SshSession;
use crate::shell::shell_escape;

/// Validated tmux session name (matches `[a-zA-Z0-9_-]+`).
///
/// The constructor rejects empty strings and any character outside the
/// allowed set, so callers that interpolate the name into a shell-built
/// `tmux new-session` command can do so without defensive quoting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TmuxSessionName(String);

impl TmuxSessionName {
    /// Parse and validate. The name must be non-empty and contain only
    /// ASCII letters, digits, `_`, or `-`.
    pub fn new(s: &str) -> Result<Self> {
        if s.is_empty() {
            bail!("tmux session name must not be empty");
        }
        for c in s.chars() {
            if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                bail!(
                    "tmux session name '{s}' contains invalid character '{c}' \
                     (allowed: a-z, A-Z, 0-9, '_', '-')"
                );
            }
        }
        Ok(Self(s.to_string()))
    }
}

impl fmt::Display for TmuxSessionName {
    #[mutants::skip] // equivalent: trivial forwarder; covered indirectly by RemoteCommand rendering tests
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for TmuxSessionName {
    #[mutants::skip] // equivalent: trivial forwarder; covered indirectly by RemoteCommand rendering tests
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for TmuxSessionName {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

/// How a remote command runs on the guest.
///
/// `Shell` runs directly in the user's login shell (empty `command`
/// opens an interactive shell). `Tmux` wraps the run inside a named
/// tmux session that survives SSH disconnects; reconnecting reattaches
/// to the existing session.
#[derive(Debug, Clone)]
pub enum RemoteCommand {
    Shell {
        command: Vec<String>,
    },
    Tmux {
        session: TmuxSessionName,
        command: Vec<String>,
    },
}

impl RemoteCommand {
    /// Render to the single string passed to `ssh ... <cmd>`.
    fn render(&self) -> String {
        match self {
            Self::Shell { command } if command.is_empty() => {
                "cd /workspace && exec $SHELL -l".to_string()
            }
            Self::Shell { command } => {
                format!("cd /workspace && {}", join_escaped(command))
            }
            Self::Tmux { session, command } if command.is_empty() => {
                format!("tmux new-session -A -s {session} -c /workspace")
            }
            Self::Tmux { session, command } => {
                format!(
                    "tmux new-session -A -s {session} -c /workspace {}",
                    shell_escape(&join_escaped(command)),
                )
            }
        }
    }
}

fn join_escaped(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ")
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
    let safe = ["xterm", "xterm-256color", "screen", "tmux", "vt100"];
    if safe.iter().any(|&s| term == s) {
        term
    } else {
        "xterm-256color".to_string()
    }
}

/// Run a `RemoteCommand` interactively over SSH with a PTY.
///
/// Used for both bare shell sessions and command launches (claude,
/// codex). The tmux variant of `RemoteCommand` enables session
/// persistence across disconnects.
pub fn run_interactive(session: &SshSession, remote: &RemoteCommand) -> Result<()> {
    let remote_cmd = remote.render();

    tracing::info!(
        "Connecting via SSH to {}:{} ({remote_cmd})",
        session.target.host,
        session.target.port,
    );

    let mut args = session.ssh_opts();
    args.push(session.target.addr());
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
#[expect(clippy::expect_used, reason = "test code — panics are assertions")]
mod tests {
    use super::*;

    fn name(s: &str) -> TmuxSessionName {
        TmuxSessionName::new(s).expect("valid tmux session name")
    }

    #[test]
    fn tmux_name_accepts_allowed_charset() {
        for s in ["main", "claude", "codex", "a", "a_b", "a-b", "ABC_123-xyz"] {
            assert!(
                TmuxSessionName::new(s).is_ok(),
                "expected '{s}' to be valid"
            );
        }
    }

    #[test]
    fn tmux_name_rejects_empty() {
        assert!(TmuxSessionName::new("").is_err());
    }

    #[test]
    fn tmux_name_rejects_shell_metacharacters() {
        for s in [
            "a b", "a;b", "a$b", "a`b", "a\"b", "a'b", "a|b", "a&b", "a/b", "a.b", "a\\b", "a\nb",
            "a*b", "a?b", "a(b", "a)b", "a{b", "a}b", "a[b", "a]b", "a<b", "a>b", "a#b", "a!b",
            "a:b", "a=b", "a%b", "a@b", "a+b",
        ] {
            assert!(
                TmuxSessionName::new(s).is_err(),
                "expected '{s}' to be rejected",
            );
        }
    }

    #[test]
    fn tmux_name_from_str() {
        let parsed: TmuxSessionName = "dev".parse().expect("valid");
        assert_eq!(parsed, name("dev"));
        assert!("bad name".parse::<TmuxSessionName>().is_err());
    }

    #[test]
    fn shell_empty_renders_interactive_shell() {
        let rc = RemoteCommand::Shell { command: vec![] };
        assert_eq!(rc.render(), "cd /workspace && exec $SHELL -l");
    }

    #[test]
    fn shell_with_command_renders_with_cd() {
        let rc = RemoteCommand::Shell {
            command: vec!["echo".into(), "hi".into()],
        };
        assert_eq!(rc.render(), "cd /workspace && 'echo' 'hi'");
    }

    #[test]
    fn tmux_empty_renders_attach() {
        let rc = RemoteCommand::Tmux {
            session: name("dev"),
            command: vec![],
        };
        assert_eq!(rc.render(), "tmux new-session -A -s dev -c /workspace");
    }

    #[test]
    fn tmux_with_command_renders_quoted_inner() {
        let rc = RemoteCommand::Tmux {
            session: name("dev"),
            command: vec!["/usr/bin/foo".into(), "--bar".into()],
        };
        // Inner command is shell-escaped (per-arg) and the whole inner
        // string is re-escaped so tmux sees one argument.
        assert_eq!(
            rc.render(),
            "tmux new-session -A -s dev -c /workspace ''\\''/usr/bin/foo'\\'' '\\''--bar'\\'''",
        );
    }

    #[test]
    fn tmux_session_name_not_escaped() {
        // The session name is a TmuxSessionName, so the constructor has
        // already rejected anything that would need escaping. The
        // rendered output uses it verbatim.
        let rc = RemoteCommand::Tmux {
            session: name("My-Session_1"),
            command: vec![],
        };
        assert!(rc.render().contains(" -s My-Session_1 "));
    }
}
