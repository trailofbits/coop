//! Helpers for parsing and validating GitHub repository identifiers.
//!
//! A *repo slug* is the canonical `owner/repo` form (e.g. `trailofbits/coop`).
//! Used as the key for `[github.pat."owner/repo"]` entries.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Validate that `slug` matches `owner/repo` with GitHub-allowed characters.
///
/// GitHub usernames and repo names allow ASCII letters, digits, `-`, `_`, `.`.
/// Leading/trailing `.` or `-` is unusual but accepted by the API; we don't
/// gate that here.
pub fn validate_repo_slug(slug: &str) -> Result<()> {
    let (owner, repo) = slug
        .split_once('/')
        .with_context(|| format!("Repo slug must be 'owner/repo', got '{slug}'"))?;
    if owner.is_empty() || repo.is_empty() {
        bail!("Repo slug 'owner/repo' must have non-empty owner and repo: '{slug}'");
    }
    if repo.contains('/') {
        bail!("Repo slug must contain exactly one '/' separator: '{slug}'");
    }
    let valid = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    if !valid(owner) || !valid(repo) {
        bail!(
            "Repo slug '{slug}' contains invalid characters \
             (allowed: a-z, A-Z, 0-9, '-', '_', '.')"
        );
    }
    Ok(())
}

/// Schemes recognised by [`parse_repo_slug_from_url`].
const GITHUB_URL_PREFIXES: &[&str] = &[
    "https://github.com/",
    "http://github.com/",
    "ssh://git@github.com/",
    "git@github.com:",
];

/// Parse a GitHub URL into an `owner/repo` slug.
///
/// Accepts:
/// - `https://github.com/owner/repo` (with or without `.git` suffix or trailing slash)
/// - `http://github.com/owner/repo`
/// - `git@github.com:owner/repo`
/// - `ssh://git@github.com/owner/repo`
///
/// Returns `None` for non-GitHub URLs or malformed input.
pub fn parse_repo_slug_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    for prefix in GITHUB_URL_PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return canonicalize(rest);
        }
    }
    None
}

fn canonicalize(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    // Reject anything past `owner/repo` (e.g. `owner/repo/pulls`).
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Detect the `owner/repo` slug for the GitHub remote of `git_dir`.
///
/// Runs `git -C <git_dir> remote get-url origin` and parses the output.
/// Returns `Ok(None)` if `git_dir` is not a git repo, has no origin, or
/// origin is not a GitHub URL.
///
/// `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` are stripped before
/// invoking git so a parent hook context (e.g. a pre-commit running
/// coop) can't redirect lookups to the wrong repository.
pub fn detect_workspace_repo(git_dir: &Path) -> Result<Option<String>> {
    if !git_dir.exists() {
        return Ok(None);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["remote", "get-url", "origin"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .with_context(|| format!("Failed to invoke git in {}", git_dir.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(parse_repo_slug_from_url(&url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_url() {
        assert_eq!(
            parse_repo_slug_from_url("https://github.com/trailofbits/coop"),
            Some("trailofbits/coop".to_string())
        );
    }

    #[test]
    fn parses_https_url_with_git_suffix() {
        assert_eq!(
            parse_repo_slug_from_url("https://github.com/trailofbits/coop.git"),
            Some("trailofbits/coop".to_string())
        );
    }

    #[test]
    fn parses_https_url_with_trailing_slash() {
        assert_eq!(
            parse_repo_slug_from_url("https://github.com/trailofbits/coop/"),
            Some("trailofbits/coop".to_string())
        );
    }

    #[test]
    fn parses_ssh_url() {
        assert_eq!(
            parse_repo_slug_from_url("git@github.com:trailofbits/coop"),
            Some("trailofbits/coop".to_string())
        );
    }

    #[test]
    fn parses_ssh_url_with_git_suffix() {
        assert_eq!(
            parse_repo_slug_from_url("git@github.com:trailofbits/coop.git"),
            Some("trailofbits/coop".to_string())
        );
    }

    #[test]
    fn parses_ssh_url_with_scheme() {
        assert_eq!(
            parse_repo_slug_from_url("ssh://git@github.com/trailofbits/coop.git"),
            Some("trailofbits/coop".to_string())
        );
    }

    #[test]
    fn parses_http_url() {
        assert_eq!(
            parse_repo_slug_from_url("http://github.com/trailofbits/coop"),
            Some("trailofbits/coop".to_string())
        );
    }

    #[test]
    fn rejects_non_github_url() {
        assert_eq!(
            parse_repo_slug_from_url("https://gitlab.com/owner/repo"),
            None
        );
    }

    #[test]
    fn rejects_deep_path() {
        assert_eq!(
            parse_repo_slug_from_url("https://github.com/owner/repo/pulls/1"),
            None
        );
    }

    #[test]
    fn rejects_only_owner() {
        assert_eq!(parse_repo_slug_from_url("https://github.com/owner"), None);
    }

    #[test]
    fn validates_canonical_slug() {
        assert!(validate_repo_slug("trailofbits/coop").is_ok());
        assert!(validate_repo_slug("user-name/repo.name_v2").is_ok());
    }

    #[test]
    fn rejects_missing_slash() {
        assert!(validate_repo_slug("trailofbits").is_err());
    }

    #[test]
    fn rejects_empty_segment() {
        assert!(validate_repo_slug("/coop").is_err());
        assert!(validate_repo_slug("trailofbits/").is_err());
    }

    #[test]
    fn rejects_extra_slash() {
        assert!(validate_repo_slug("a/b/c").is_err());
    }

    #[test]
    fn rejects_bad_chars() {
        assert!(validate_repo_slug("owner!/repo").is_err());
        assert!(validate_repo_slug("owner repo/x").is_err());
    }
}
