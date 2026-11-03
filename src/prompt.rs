//! Shared interactive confirmation and free-form terminal prompts.
//!
//! Reads from stdin / writes to stderr. Returns `Ok(false)` immediately when
//! stdin is not a TTY — non-interactive callers must opt in with their own
//! `--yes`-style flag rather than getting silently confirmed.

use std::io::IsTerminal as _;
use std::os::fd::AsRawFd as _;

use anyhow::{Context, Result};

pub(crate) const CONFIRM_MAX_BYTES: usize = 64;
pub(crate) const MODEL_NAME_MAX_BYTES: usize = 1024;
pub(crate) const URL_MAX_BYTES: usize = 8192;
pub(crate) const SECRET_MAX_BYTES: usize = 65_536;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum BoundedLine {
    Eof,
    Empty,
    Value(String),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BoundedLineError {
    #[error("input exceeds the {limit}-byte limit")]
    TooLong { limit: usize },
    #[error("input is not valid UTF-8")]
    InvalidUtf8,
    #[error("failed to read input")]
    Io(#[source] std::io::Error),
}

pub(crate) fn read_bounded_line(
    input: &mut impl std::io::BufRead,
    limit: usize,
) -> std::result::Result<BoundedLine, BoundedLineError> {
    let sentinel_limit = limit.saturating_add(1);
    let mut bytes = Vec::with_capacity(sentinel_limit.min(4096));
    let mut saw_input = false;
    let mut pending_carriage_return = false;
    loop {
        let available = input.fill_buf().map_err(BoundedLineError::Io)?;
        let Some(&byte) = available.first() else {
            if pending_carriage_return {
                bytes.push(b'\r');
            }
            break;
        };
        input.consume(1);
        saw_input = true;
        if byte == b'\n' {
            break;
        }
        if pending_carriage_return {
            bytes.push(b'\r');
            pending_carriage_return = false;
            if bytes.len() > limit {
                return Err(BoundedLineError::TooLong { limit });
            }
        }
        if byte == b'\r' {
            pending_carriage_return = true;
        } else {
            bytes.push(byte);
        }
        if bytes.len() > limit {
            return Err(BoundedLineError::TooLong { limit });
        }
    }

    if !saw_input {
        return Ok(BoundedLine::Eof);
    }
    if bytes.len() > limit {
        return Err(BoundedLineError::TooLong { limit });
    }
    let value = String::from_utf8(bytes).map_err(|_| BoundedLineError::InvalidUtf8)?;
    if value.is_empty() {
        Ok(BoundedLine::Empty)
    } else {
        Ok(BoundedLine::Value(value))
    }
}

pub fn confirm(prompt: &str) -> Result<bool> {
    let stdin = std::io::stdin();
    let is_terminal = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut stderr = std::io::stderr();
    confirm_with(prompt, is_terminal, &mut input, &mut stderr)
}

fn confirm_with(
    prompt: &str,
    is_terminal: bool,
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
) -> Result<bool> {
    if !is_terminal {
        return Ok(false);
    }
    write!(output, "{prompt} [y/N] ").context("Failed to write confirmation prompt")?;
    output.flush().context("Failed to flush stderr")?;
    let response =
        read_bounded_line(input, CONFIRM_MAX_BYTES).context("Failed to read confirmation")?;
    Ok(matches!(response, BoundedLine::Value(value) if is_affirmative(&value)))
}

fn is_affirmative(response: &str) -> bool {
    matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Prompt for a free-form line of input. Returns the trimmed reply, or
/// `None` when stdin is not a TTY or the reply is empty (so callers can
/// treat empty as "skip"/"use default").
pub fn read_line(prompt: &str, max_bytes: usize) -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    read_line_any(prompt, max_bytes)
}

/// Read a bounded line from either a terminal or redirected stdin.
pub fn read_line_any(prompt: &str, max_bytes: usize) -> Result<Option<String>> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut stderr = std::io::stderr();
    read_line_with(prompt, max_bytes, &mut input, &mut stderr)
}

fn read_line_with(
    prompt: &str,
    max_bytes: usize,
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
) -> Result<Option<String>> {
    write!(output, "{prompt}: ").context("Failed to write prompt")?;
    output.flush().context("Failed to flush stderr")?;
    let response = read_bounded_line(input, max_bytes).context("Failed to read input")?;
    Ok(optional_trimmed(response))
}

/// Read a bounded secret from stdin. Terminal echo is disabled using the
/// current terminal's exact saved attributes; redirected stdin remains
/// supported and receives the same byte limit.
pub fn read_secret(prompt: &str, max_bytes: usize) -> Result<Option<String>> {
    let stdin = std::io::stdin();
    let is_terminal = stdin.is_terminal();
    let fd = stdin.as_raw_fd();
    let mut input = stdin.lock();
    let mut stderr = std::io::stderr();
    read_secret_with(prompt, max_bytes, is_terminal, fd, &mut input, &mut stderr)
}

fn optional_trimmed(line: BoundedLine) -> Option<String> {
    let BoundedLine::Value(value) = line else {
        return None;
    };
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn read_secret_with(
    prompt: &str,
    max_bytes: usize,
    is_terminal: bool,
    terminal_fd: std::os::fd::RawFd,
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
) -> Result<Option<String>> {
    write!(output, "{prompt}: ").context("Failed to write secret prompt")?;
    output.flush().context("Failed to flush secret prompt")?;
    let mut guard = EchoGuard::disable(terminal_fd, is_terminal)
        .context("Failed to disable terminal echo for secret input")?;
    let line = read_bounded_line(input, max_bytes);
    if guard.is_active() {
        writeln!(output).context("Failed to finish secret prompt")?;
        output.flush().context("Failed to flush secret prompt")?;
    }
    guard
        .restore_inner()
        .context("Failed to restore terminal state after secret input")?;
    Ok(optional_trimmed(
        line.context("Failed to read secret input")?,
    ))
}

struct EchoGuard {
    fd: std::os::fd::RawFd,
    original: Option<libc::termios>,
}

impl EchoGuard {
    fn disable(fd: std::os::fd::RawFd, is_terminal: bool) -> std::io::Result<Self> {
        if !is_terminal {
            return Ok(Self { fd, original: None });
        }
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `original` points to writable termios storage and `fd` was
        // obtained from the live stdin handle.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: tcgetattr initialized `original` on success.
        let original = unsafe { original.assume_init() };
        let mut private = original;
        private.c_lflag &= !libc::ECHO;
        // SAFETY: `private` is a valid termios value for this terminal fd.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const private) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            original: Some(original),
        })
    }

    fn is_active(&self) -> bool {
        self.original.is_some()
    }

    fn restore_inner(&mut self) -> std::io::Result<()> {
        let Some(original) = self.original.as_ref() else {
            return Ok(());
        };
        // SAFETY: `original` came from tcgetattr on this fd.
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, original) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.original = None;
        Ok(())
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test fixtures use infallible in-memory IO"
)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;

    struct ReadyReader<R> {
        inner: R,
        ready: Option<std::sync::mpsc::Sender<()>>,
    }

    impl<R: std::io::Read> std::io::Read for ReadyReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(ready) = self.ready.take() {
                ready.send(()).unwrap();
            }
            self.inner.read(buf)
        }
    }

    impl<R: std::io::BufRead> std::io::BufRead for ReadyReader<R> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if let Some(ready) = self.ready.take() {
                ready.send(()).unwrap();
            }
            self.inner.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.inner.consume(amount);
        }
    }

    #[test]
    fn confirmation_requires_explicit_yes() {
        for yes in ["y", "Y", "yes", "YES", " yes \n"] {
            assert!(is_affirmative(yes), "expected affirmative reply: {yes:?}");
        }
        for no in ["", "\n", " ", "n", "no", "maybe", "yes please"] {
            assert!(!is_affirmative(no), "expected refusal: {no:?}");
        }
    }

    #[test]
    fn non_terminal_confirmation_declines_without_reading_or_prompting() {
        let mut input = Cursor::new(b"yes\n".to_vec());
        let mut output = Vec::new();

        assert!(!confirm_with("Proceed", false, &mut input, &mut output).unwrap());
        assert_eq!(input.position(), 0, "non-TTY input must not be consumed");
        assert!(output.is_empty(), "non-TTY confirmation must not prompt");
    }

    #[test]
    fn terminal_eof_and_empty_line_decline() {
        for reply in [b"".as_slice(), b"\n".as_slice()] {
            let mut input = Cursor::new(reply);
            let mut output = Vec::new();

            assert!(!confirm_with("Proceed", true, &mut input, &mut output).unwrap());
            assert_eq!(output, b"Proceed [y/N] ");
        }
    }

    #[test]
    fn terminal_yes_approves() {
        let mut input = Cursor::new(b"yes\n".as_slice());
        let mut output = Vec::new();

        assert!(confirm_with("Proceed", true, &mut input, &mut output).unwrap());
        assert_eq!(output, b"Proceed [y/N] ");
    }

    #[test]
    fn bounded_line_distinguishes_eof_empty_value_limit_and_encoding() {
        assert!(matches!(
            read_bounded_line(&mut Cursor::new(b""), 4).unwrap(),
            BoundedLine::Eof
        ));
        assert!(matches!(
            read_bounded_line(&mut Cursor::new(b"\n"), 4).unwrap(),
            BoundedLine::Empty
        ));
        assert!(matches!(
            read_bounded_line(&mut Cursor::new(b"abcd\n"), 4).unwrap(),
            BoundedLine::Value(value) if value == "abcd"
        ));
        assert!(matches!(
            read_bounded_line(&mut Cursor::new(b"abcd\r\n"), 4).unwrap(),
            BoundedLine::Value(value) if value == "abcd"
        ));
        assert!(matches!(
            read_bounded_line(&mut Cursor::new(b"abc\r\r\n"), 4).unwrap(),
            BoundedLine::Value(value) if value == "abc\r"
        ));
        assert!(matches!(
            read_bounded_line(&mut Cursor::new(b"\rX\n"), 4).unwrap(),
            BoundedLine::Value(value) if value == "\rX"
        ));
        assert!(matches!(
            read_bounded_line(&mut Cursor::new(b"abcd"), 4).unwrap(),
            BoundedLine::Value(value) if value == "abcd"
        ));
        assert!(matches!(
            read_bounded_line(&mut Cursor::new(b"abcde\n"), 4),
            Err(BoundedLineError::TooLong { limit: 4 })
        ));
        assert!(matches!(
            read_bounded_line(&mut Cursor::new(b"abcd\rX\n"), 4),
            Err(BoundedLineError::TooLong { limit: 4 })
        ));
        assert!(matches!(
            read_bounded_line(&mut Cursor::new([0xff, b'\n']), 4),
            Err(BoundedLineError::InvalidUtf8)
        ));
    }

    #[test]
    fn bounded_line_stops_after_one_sentinel_byte_without_a_newline() {
        let mut input = BufReader::with_capacity(1, std::io::repeat(b'x'));
        assert!(matches!(
            read_bounded_line(&mut input, 64),
            Err(BoundedLineError::TooLong { limit: 64 })
        ));
    }

    #[test]
    fn redirected_secret_input_is_accepted_and_bounded() {
        let mut input = Cursor::new(b"pipe-secret\n");
        let mut output = Vec::new();
        let secret = read_secret_with("Token", 16, false, -1, &mut input, &mut output).unwrap();
        assert_eq!(secret.as_deref(), Some("pipe-secret"));
        assert_eq!(output, b"Token: ");

        let mut oversized = Cursor::new(b"12345\n");
        let error =
            read_secret_with("Token", 4, false, -1, &mut oversized, &mut Vec::new()).unwrap_err();
        assert!(format!("{error:#}").contains("4-byte limit"));
    }

    #[test]
    fn redirected_secret_and_following_menu_choice_share_bounded_input() {
        let mut input = Cursor::new(b"pipe-secret\n1\n");
        let secret = read_secret_with("Token", 64, false, -1, &mut input, &mut Vec::new()).unwrap();
        let choice = read_line_with("Choice", 64, &mut input, &mut Vec::new()).unwrap();
        assert_eq!(secret.as_deref(), Some("pipe-secret"));
        assert_eq!(choice.as_deref(), Some("1"));
    }

    #[test]
    fn terminal_echo_is_disabled_and_exact_flags_are_restored() {
        use std::fs::File;
        use std::io::{Read as _, Write as _};
        use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
        use std::sync::mpsc;
        use std::time::Duration;

        fn attributes(fd: RawFd) -> libc::termios {
            let mut value = std::mem::MaybeUninit::<libc::termios>::uninit();
            // SAFETY: value is writable and openpty returned a live tty fd.
            assert_eq!(unsafe { libc::tcgetattr(fd, value.as_mut_ptr()) }, 0);
            // SAFETY: tcgetattr succeeded and initialized value.
            unsafe { value.assume_init() }
        }

        let mut master = -1;
        let mut slave = -1;
        // SAFETY: output pointers are valid and null optional arguments request
        // the platform's default terminal attributes and window size.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let before = attributes(slave);
        assert_ne!(before.c_lflag & libc::ECHO, 0);

        // SAFETY: ownership of each fresh openpty descriptor is transferred
        // exactly once to File.
        let slave_file = unsafe { File::from_raw_fd(slave) };
        // SAFETY: same ownership transfer for the master descriptor.
        let mut master_file = unsafe { File::from_raw_fd(master) };
        let (ready_tx, ready_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            ready_rx.recv().unwrap();
            master_file.write_all(b"private-token\n").unwrap();
            std::thread::sleep(Duration::from_millis(20));
            let flags = unsafe { libc::fcntl(master_file.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe {
                    libc::fcntl(
                        master_file.as_raw_fd(),
                        libc::F_SETFL,
                        flags | libc::O_NONBLOCK,
                    )
                },
                0
            );
            let mut echoed = Vec::new();
            let _ = master_file.read_to_end(&mut echoed);
            (master_file, echoed)
        });

        let mut input = ReadyReader {
            inner: BufReader::new(slave_file),
            ready: Some(ready_tx),
        };
        let mut output = Vec::new();
        let secret = read_secret_with("Token", 64, true, slave, &mut input, &mut output).unwrap();
        assert_eq!(secret.as_deref(), Some("private-token"));
        assert_eq!(output, b"Token: \n");
        let (_master_file, echoed) = writer.join().unwrap();
        assert!(!echoed.windows(13).any(|w| w == b"private-token"));

        let after = attributes(slave);
        assert_eq!(after.c_iflag, before.c_iflag);
        assert_eq!(after.c_oflag, before.c_oflag);
        assert_eq!(after.c_cflag, before.c_cflag);
        assert_eq!(after.c_lflag, before.c_lflag);
        assert_eq!(after.c_cc, before.c_cc);

        let mut invalid = Cursor::new([0xff, b'\n']);
        assert!(read_secret_with("Token", 64, true, slave, &mut invalid, &mut Vec::new()).is_err());
        let after_error = attributes(slave);
        assert_eq!(after_error.c_iflag, before.c_iflag);
        assert_eq!(after_error.c_oflag, before.c_oflag);
        assert_eq!(after_error.c_cflag, before.c_cflag);
        assert_eq!(after_error.c_lflag, before.c_lflag);
        assert_eq!(after_error.c_cc, before.c_cc);
    }
}
