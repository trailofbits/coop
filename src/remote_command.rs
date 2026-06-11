//! A typed builder for the shell command strings sent over SSH.
//!
//! Remote commands are concatenated with spaces by `ssh` and handed to the
//! guest's shell, so any dynamic value spliced into one must be escaped or it
//! becomes live shell syntax (the #301 bug class). [`RemoteCommand`] makes that
//! escaping happen *by construction*: the only way to add an untrusted value is
//! [`RemoteCommand::arg`], which runs [`crate::shell::shell_escape`] exactly
//! once. Trusted shell text coop authors itself — control flow, fixed flags,
//! pipelines — goes through [`RemoteCommand::literal`], the explicit and
//! auditable escape hatch.
//!
//! Because [`SshTarget::exec`](crate::backend::SshTarget::exec) and its
//! siblings take a `RemoteCommand` rather than a `&str`, a bare `format!`
//! string no longer type-checks at the SSH boundary — so the two failure modes
//! are unrepresentable rather than merely discouraged:
//!
//! 1. forgetting to escape an interpolated value, and
//! 2. double-escaping a value that was already escaped.
//!
//! # Composition model
//!
//! Parts are concatenated verbatim — there is no implicit separator. A
//! [`RemoteCommand`] reads like the `format!` string it replaces, with each
//! `{shell_escape(x)}` becoming `.arg(x)` and the surrounding text becoming
//! `.literal(...)`. The caller keeps whatever spacing the command needs, which
//! is what lets a value glue directly onto adjacent text — e.g.
//! `.arg(dir).literal("/.git")` renders `'<dir>'/.git`, a single shell word.
//!
//! ```ignore
//! let cmd = RemoteCommand::new()
//!     .literal("tar xf - -C ")
//!     .arg(guest_path);            // escaped exactly once, by construction
//! target.exec(cmd)?;
//! ```

use crate::shell::shell_escape;

/// A remote shell command assembled from trusted literal fragments and
/// untrusted values that are shell-escaped exactly once.
///
/// See the [module docs](self) for the composition model and rationale.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteCommand {
    rendered: String,
}

impl RemoteCommand {
    /// An empty command. Build it up with [`literal`](Self::literal) and
    /// [`arg`](Self::arg).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a trusted literal shell fragment authored by coop — control
    /// flow, fixed flags, pipelines, redirections.
    ///
    /// Never pass a dynamic or user-supplied value here; that is what
    /// [`arg`](Self::arg) is for. The fragment is appended verbatim, so it
    /// carries its own spacing.
    #[must_use]
    pub fn literal(mut self, fragment: impl AsRef<str>) -> Self {
        self.rendered.push_str(fragment.as_ref());
        self
    }

    /// Append an untrusted value, shell-escaped exactly once so it reaches the
    /// remote shell as a single literal word regardless of its contents.
    ///
    /// The value is appended verbatim after escaping — include any needed
    /// separating space in the surrounding [`literal`](Self::literal) calls.
    #[must_use]
    pub fn arg(mut self, value: impl AsRef<str>) -> Self {
        self.rendered.push_str(&shell_escape(value.as_ref()));
        self
    }

    /// Consume the builder and return the assembled command string for the
    /// `ssh` argument vector.
    #[must_use]
    pub fn into_string(self) -> String {
        self.rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_command_renders_empty() {
        assert_eq!(RemoteCommand::new().into_string(), "");
    }

    #[test]
    fn literal_is_passed_through_verbatim() {
        let cmd = RemoteCommand::new().literal("gh auth setup-git");
        assert_eq!(cmd.into_string(), "gh auth setup-git");
    }

    #[test]
    fn literal_does_not_escape_shell_metacharacters() {
        // Literals are trusted shell text — `~`, `&&`, `$()` must survive so
        // the remote shell interprets them, not receive them quoted.
        let cmd = RemoteCommand::new().literal("mkdir -p ~/.coop && echo $(whoami)");
        assert_eq!(cmd.into_string(), "mkdir -p ~/.coop && echo $(whoami)");
    }

    #[test]
    fn arg_single_quotes_the_value() {
        let cmd = RemoteCommand::new()
            .literal("test -x ")
            .arg("/usr/local/bin/codex");
        assert_eq!(cmd.into_string(), "test -x '/usr/local/bin/codex'");
    }

    #[test]
    fn arg_neutralizes_injection() {
        // A value carrying shell metacharacters must end up inside the quotes,
        // never as live syntax.
        let cmd = RemoteCommand::new()
            .literal("git clone ")
            .arg("/work; rm -rf / $(touch pwned)")
            .literal(" /workspace");
        assert_eq!(
            cmd.into_string(),
            "git clone '/work; rm -rf / $(touch pwned)' /workspace"
        );
    }

    #[test]
    fn arg_escapes_embedded_single_quote_once() {
        // Exactly one round of escaping: the `'\''` trick, not double-wrapped.
        let cmd = RemoteCommand::new().arg("o'wner");
        assert_eq!(cmd.into_string(), "'o'\\''wner'");
    }

    #[test]
    fn arg_glues_directly_onto_following_literal() {
        // The `{guest_path}/.git` adjacency from `check_guest_dirty`: the
        // escaped value and the trailing literal form one shell word.
        let cmd = RemoteCommand::new()
            .literal("[ -d ")
            .arg("/my work")
            .literal("/.git ]");
        assert_eq!(cmd.into_string(), "[ -d '/my work'/.git ]");
    }

    #[test]
    fn same_value_used_twice_escapes_each_occurrence() {
        let cmd = RemoteCommand::new()
            .literal("mkdir -p ")
            .arg("/a b")
            .literal(" && chown ubuntu ")
            .arg("/a b");
        assert_eq!(cmd.into_string(), "mkdir -p '/a b' && chown ubuntu '/a b'");
    }
}
