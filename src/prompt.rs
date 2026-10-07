//! Interactive y/N prompt used by `coop update` and `coop uninstall`.
//!
//! Reads from stdin / writes to stderr. Returns `Ok(false)` immediately when
//! stdin is not a TTY — non-interactive callers must opt in with their own
//! `--yes`-style flag rather than getting silently confirmed.

use std::io::{IsTerminal as _, Write as _};

use anyhow::{Context, Result};

pub fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let mut stderr = std::io::stderr();
    write!(stderr, "{prompt} [y/N] ").context("Failed to write confirmation prompt")?;
    stderr.flush().context("Failed to flush stderr")?;
    let mut response = String::new();
    std::io::stdin()
        .read_line(&mut response)
        .context("Failed to read confirmation")?;
    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
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
