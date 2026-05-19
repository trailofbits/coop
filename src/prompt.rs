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

/// Variant of [`confirm`] where the default (empty reply) is *yes*.
///
/// Used for "Use devcontainer.json?" — the answer is yes for any user
/// who set the file up in the first place; this saves a keystroke without
/// hiding the choice. Returns `Ok(false)` when stdin is not a TTY — the
/// caller is responsible for surfacing a more specific non-interactive
/// error if they want different behaviour.
pub fn confirm_default_yes(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let mut stderr = std::io::stderr();
    write!(stderr, "{prompt} [Y/n] ").context("Failed to write confirmation prompt")?;
    stderr.flush().context("Failed to flush stderr")?;
    let mut response = String::new();
    std::io::stdin()
        .read_line(&mut response)
        .context("Failed to read confirmation")?;
    match response.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => Ok(true),
        _ => Ok(false),
    }
}
