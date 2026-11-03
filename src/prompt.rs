//! Shared interactive confirmation and free-form terminal prompts.
//!
//! Reads from stdin / writes to stderr. Returns `Ok(false)` immediately when
//! stdin is not a TTY — non-interactive callers must opt in with their own
//! `--yes`-style flag rather than getting silently confirmed.

use std::io::{IsTerminal as _, Write as _};

use anyhow::{Context, Result};

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
    let mut response = String::new();
    input
        .read_line(&mut response)
        .context("Failed to read confirmation")?;
    Ok(is_affirmative(&response))
}

fn is_affirmative(response: &str) -> bool {
    matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Prompt for a free-form line of input. Returns the trimmed reply, or
/// `None` when stdin is not a TTY or the reply is empty (so callers can
/// treat empty as "skip"/"use default").
pub fn read_line(prompt: &str) -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut stderr = std::io::stderr();
    write!(stderr, "{prompt}: ").context("Failed to write prompt")?;
    stderr.flush().context("Failed to flush stderr")?;
    let mut response = String::new();
    std::io::stdin()
        .read_line(&mut response)
        .context("Failed to read input")?;
    let trimmed = response.trim();
    Ok(if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    })
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test fixtures use infallible in-memory IO"
)]
mod tests {
    use std::io::Cursor;

    use super::*;

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
}
