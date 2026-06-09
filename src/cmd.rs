use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// A command-line argument paired with its redaction status.
///
/// Pairing the value with the variant means a redacted arg cannot be
/// silently separated from its redaction marker by reordering or
/// insertion — the contract travels with the value.
enum Arg {
    Public(OsString),
    Redacted(OsString),
}

impl Arg {
    fn value(&self) -> &OsStr {
        match self {
            Self::Public(v) | Self::Redacted(v) => v,
        }
    }

    /// String form for human-facing output. Redacted args yield the
    /// placeholder, so the redaction policy lives with the type instead
    /// of being re-derived at every formatting site.
    ///
    /// Not `Display` on purpose: a `Display` impl that silently swaps
    /// content is a footgun for any future caller who reaches for `{}`.
    fn display(&self) -> Cow<'_, str> {
        match self {
            Self::Public(v) => v.to_string_lossy(),
            Self::Redacted(_) => Cow::Borrowed("<redacted>"),
        }
    }
}

/// Builder for running external commands with consistent logging,
/// error checking, and optional sudo elevation.
///
/// Replaces the scattered `run_cmd()`, `sudo()`, and inline
/// `Command::new()` patterns with a single fluent API.
pub struct Cmd {
    program: OsString,
    args: Vec<Arg>,
    sudo: bool,
    stdin: Option<Vec<u8>>,
}

impl Cmd {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            sudo: false,
            stdin: None,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(Arg::Public(arg.as_ref().to_owned()));
        self
    }

    /// Add an argument that holds a secret, redacted in [`Cmd::describe`]
    /// (and therefore in debug logs). The bytes still appear on argv —
    /// callers should prefer [`Cmd::stdin_input`] when the tool supports
    /// reading from stdin. Use this only for tools that have no
    /// stdin-friendly path (e.g. macOS `security add-generic-password -w`).
    pub fn redacted_arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(Arg::Redacted(arg.as_ref().to_owned()));
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|s| Arg::Public(s.as_ref().to_owned())));
        self
    }

    /// Run the command under `sudo`.
    pub fn sudo(mut self) -> Self {
        self.sudo = true;
        self
    }

    /// Pipe `bytes` to the child's stdin instead of inheriting the parent's.
    ///
    /// Use this for any data that must NOT appear on argv — secrets, tokens,
    /// or large payloads. The bytes are written and stdin is closed before
    /// the child exits, so the child sees EOF without further input.
    ///
    /// Compatible with [`Cmd::run`] and [`Cmd::capture`]; the existing
    /// [`Cmd::stdin_write`] takes its data per-call instead.
    pub fn stdin_input(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    /// Build the underlying `Command` for complex use cases that
    /// need custom stdio, spawn, or other `Command` methods.
    pub fn build(&self) -> Command {
        let raw_args = self.args.iter().map(Arg::value);
        if self.sudo {
            let mut cmd = Command::new("sudo");
            cmd.arg(&self.program);
            cmd.args(raw_args);
            cmd
        } else {
            let mut cmd = Command::new(&self.program);
            cmd.args(raw_args);
            cmd
        }
    }

    fn describe(&self) -> String {
        let prefix = if self.sudo { "sudo " } else { "" };
        let prog = self.program.to_string_lossy();
        if self.args.is_empty() {
            format!("{prefix}{prog}")
        } else {
            let args: Vec<Cow<'_, str>> = self.args.iter().map(Arg::display).collect();
            format!("{prefix}{prog} {}", args.join(" "))
        }
    }

    /// Run the command and check for a successful exit status.
    pub fn run(&self) -> Result<()> {
        let desc = self.describe();
        tracing::debug!("Running: {desc}");
        let status = match self.stdin.as_deref() {
            None => self
                .build()
                .status()
                .with_context(|| format!("Failed to execute {desc}"))?,
            Some(bytes) => {
                let mut child = self
                    .build()
                    .stdin(Stdio::piped())
                    .spawn()
                    .with_context(|| format!("Failed to start {desc}"))?;
                write_stdin_and_close(&mut child, bytes, &desc)?;
                child
                    .wait()
                    .with_context(|| format!("Failed to wait for {desc}"))?
            }
        };
        if !status.success() {
            bail!("{desc} exited with {status}");
        }
        Ok(())
    }

    /// Pipe `data` into stdin, suppress stdout, and check exit status.
    pub fn stdin_write(&self, data: &[u8]) -> Result<()> {
        let desc = self.describe();
        tracing::debug!("Running (stdin pipe): {desc}");
        let mut child = self
            .build()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .with_context(|| format!("Failed to start {desc}"))?;
        write_stdin_and_close(&mut child, data, &desc)?;
        let status = child
            .wait()
            .with_context(|| format!("Failed to wait for {desc}"))?;
        if !status.success() {
            bail!("{desc} exited with {status}");
        }
        Ok(())
    }

    /// Run the command and return the full `Output` (stdout, stderr,
    /// exit status). Does not check exit status — the caller decides
    /// how to handle failure.
    pub fn output(&self) -> Result<std::process::Output> {
        let desc = self.describe();
        tracing::debug!("Running (output): {desc}");
        self.build()
            .output()
            .with_context(|| format!("Failed to execute {desc}"))
    }

    /// Run the command and capture stdout as a `String`.
    /// Fails if the command exits non-zero or stdout is not UTF-8.
    pub fn capture(&self) -> Result<String> {
        let desc = self.describe();
        tracing::debug!("Running (capture): {desc}");
        let output = match self.stdin.as_deref() {
            None => self
                .build()
                .stderr(Stdio::null())
                .output()
                .with_context(|| format!("Failed to execute {desc}"))?,
            Some(bytes) => {
                let mut child = self
                    .build()
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .with_context(|| format!("Failed to start {desc}"))?;
                write_stdin_and_close(&mut child, bytes, &desc)?;
                child
                    .wait_with_output()
                    .with_context(|| format!("Failed to wait for {desc}"))?
            }
        };
        if !output.status.success() {
            bail!("{desc} exited with {}", output.status);
        }
        String::from_utf8(output.stdout)
            .with_context(|| format!("{desc} produced non-UTF-8 output"))
    }

    /// Run the command with stdout and stderr suppressed.
    /// Returns `true` if the exit status is success.
    ///
    /// `stdin_input` is ignored here — `status_ok` is used for probes
    /// (`command -v gh`, `gh auth status`) that take no input.
    pub fn status_ok(&self) -> bool {
        let desc = self.describe();
        tracing::debug!("Running (status_ok): {desc}");
        self.build()
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// Write `bytes` to the child's stdin, then close it so the child sees EOF.
///
/// Used by [`Cmd::run`] and [`Cmd::capture`] when [`Cmd::stdin_input`] was
/// configured. Closing the handle is what curl `-H @-` and similar idioms
/// rely on to know the input is complete.
fn write_stdin_and_close(child: &mut std::process::Child, bytes: &[u8], desc: &str) -> Result<()> {
    let mut stdin = child
        .stdin
        .take()
        .with_context(|| format!("stdin pipe missing for {desc}"))?;
    stdin
        .write_all(bytes)
        .with_context(|| format!("Failed to write stdin for {desc}"))?;
    // Dropping `stdin` closes the pipe so the child sees EOF.
    drop(stdin);
    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code — panics are assertions")]
mod tests {
    use super::*;

    #[test]
    fn cmd_describe_without_sudo() {
        let cmd = Cmd::new("tar").args(["-xzf", "file.tar.gz"]);
        assert_eq!(cmd.describe(), "tar -xzf file.tar.gz");
    }

    #[test]
    fn cmd_describe_with_sudo() {
        let cmd = Cmd::new("mount").args(["-o", "loop"]).sudo();
        assert_eq!(cmd.describe(), "sudo mount -o loop");
    }

    #[test]
    fn cmd_describe_no_args() {
        let cmd = Cmd::new("ls");
        assert_eq!(cmd.describe(), "ls");
    }

    #[test]
    fn cmd_describe_redacts_only_redacted_arg() {
        let cmd = Cmd::new("op")
            .arg("item")
            .arg("create")
            .redacted_arg("password=hunter2")
            .arg("--title=demo");
        assert_eq!(cmd.describe(), "op item create <redacted> --title=demo");
    }

    #[test]
    fn cmd_describe_redacts_each_redacted_arg() {
        let cmd = Cmd::new("tool")
            .redacted_arg("secret-a")
            .arg("between")
            .redacted_arg("secret-b");
        assert_eq!(cmd.describe(), "tool <redacted> between <redacted>");
    }

    #[test]
    fn cmd_build_passes_redacted_value_to_command() {
        let cmd = Cmd::new("echo")
            .arg("public")
            .redacted_arg("sensitive-payload");
        let out = cmd.capture().expect("echo capture should succeed");
        assert!(
            out.contains("public") && out.contains("sensitive-payload"),
            "echo should receive both args verbatim: {out}",
        );
    }

    #[test]
    fn stdin_input_round_trips_through_capture() {
        // `cat` echoes stdin to stdout — verifies the pipe is wired and closed.
        let out = Cmd::new("cat").stdin_input(b"hello".to_vec()).capture();
        assert_eq!(out.expect("cat capture"), "hello");
    }

    #[test]
    fn stdin_input_run_succeeds_for_consumer_command() {
        // `cat` exits 0 after consuming stdin; closing the pipe must signal EOF
        // so it does not hang.
        Cmd::new("cat")
            .stdin_input(b"data".to_vec())
            .run()
            .expect("cat run");
    }
}
