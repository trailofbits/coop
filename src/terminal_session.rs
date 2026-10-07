use anyhow::{Result, bail};
use clap::ValueEnum;

use crate::config;

/// Terminal session backend for long-running interactive commands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum TerminalSessionKind {
    /// Plain SSH PTY, with no terminal multiplexer.
    #[default]
    Direct,
    /// Attach to or create a tmux session in the guest.
    Tmux,
    /// Attach to or create a Zellij session in the guest.
    Zellij,
}

impl TerminalSessionKind {
    pub(crate) fn is_direct(self) -> bool {
        matches!(self, Self::Direct)
    }
}

/// Conservative session suffix accepted from the CLI.
///
/// The full session name is still shell-escaped before it crosses SSH. This
/// type keeps tmux/Zellij identities readable and portable across both tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalSessionName(String);

impl TerminalSessionName {
    pub(crate) fn parse_cli(name: &str) -> Result<Self> {
        if name.is_empty() {
            bail!("session name must not be empty");
        }
        if name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            Ok(Self(name.to_owned()))
        } else {
            bail!("session name must contain only letters, digits, '_', '-', or '.'");
        }
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InteractiveLaunch {
    Shell,
    Claude,
    ClaudeAgents,
    Codex,
    Grok,
}

impl InteractiveLaunch {
    fn slug(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Claude => "claude",
            Self::ClaudeAgents => "claude-agents",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalSession {
    pub(crate) kind: TerminalSessionKind,
    pub(crate) name: String,
    pub(crate) title: String,
}

/// CLI fields shared by long-running interactive launch commands.
#[derive(clap::Args, Clone, Debug, Default)]
pub(crate) struct TerminalSessionCli {
    /// Run inside a persistent guest terminal session
    #[arg(long = "session", value_enum, default_value = "direct")]
    pub(crate) kind: TerminalSessionKind,
    /// Create or reconnect to a distinct named session
    #[arg(long = "session-name", value_parser = TerminalSessionName::parse_cli)]
    pub(crate) session_name: Option<TerminalSessionName>,
    /// Visual title for the multiplexer window/tab/pane
    #[arg(long = "session-title", value_name = "TITLE")]
    pub(crate) title: Option<String>,
}

impl TerminalSessionCli {
    pub(crate) fn into_launch(
        self,
        instance: &config::InstanceName,
        launch: InteractiveLaunch,
    ) -> Result<Option<TerminalSession>> {
        if self.kind.is_direct() {
            if self.session_name.is_some() || self.title.is_some() {
                bail!(
                    "--session-name and --session-title require --session tmux or --session zellij"
                );
            }
            return Ok(None);
        }

        let slug = launch.slug();
        let base = format!("coop-{}-{slug}", instance.as_str());
        let name = match self.session_name {
            Some(name) => format!("{base}-{}", name.as_str()),
            None => base,
        };
        let title = self
            .title
            .unwrap_or_else(|| format!("coop:{}:{slug}", instance.as_str()));
        Ok(Some(TerminalSession {
            kind: self.kind,
            name,
            title,
        }))
    }

    pub(crate) fn ensure_direct_for_short_command(&self, command: &str) -> Result<()> {
        if self.kind.is_direct() && self.session_name.is_none() && self.title.is_none() {
            return Ok(());
        }
        bail!(
            "--session applies only to long-running interactive sessions; `{command}` is not wrapped"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_session_identity_uses_instance_and_launch_kind() {
        let inst = config::InstanceName::new("demo").expect("instance");
        let opts = TerminalSessionCli {
            kind: TerminalSessionKind::Tmux,
            session_name: None,
            title: None,
        };
        let session = opts
            .into_launch(&inst, InteractiveLaunch::Claude)
            .expect("session")
            .expect("wrapped");
        assert_eq!(session.name, "coop-demo-claude");
        assert_eq!(session.title, "coop:demo:claude");
    }

    #[test]
    fn explicit_session_name_is_a_suffix_and_title_is_visual_only() {
        let inst = config::InstanceName::new("demo").expect("instance");
        let opts = TerminalSessionCli {
            kind: TerminalSessionKind::Zellij,
            session_name: Some(TerminalSessionName::parse_cli("review").expect("name")),
            title: Some("review fixes".into()),
        };
        let session = opts
            .into_launch(&inst, InteractiveLaunch::Codex)
            .expect("session")
            .expect("wrapped");
        assert_eq!(session.name, "coop-demo-codex-review");
        assert_eq!(session.title, "review fixes");
    }

    #[test]
    fn direct_rejects_title_without_mux() {
        let inst = config::InstanceName::new("demo").expect("instance");
        let opts = TerminalSessionCli {
            kind: TerminalSessionKind::Direct,
            session_name: None,
            title: Some("title".into()),
        };
        let err = opts
            .into_launch(&inst, InteractiveLaunch::Shell)
            .expect_err("title needs mux");
        assert!(err.to_string().contains("require --session"));
    }

    #[test]
    fn session_name_rejects_shell_metacharacters() {
        let err = TerminalSessionName::parse_cli("bad;name").expect_err("invalid name");
        assert!(err.to_string().contains("letters"));
    }
}
