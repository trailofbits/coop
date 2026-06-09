//! Helpers for parsing and validating GitHub repository identifiers.
//!
//! A *repo slug* is the canonical `owner/repo` form (e.g. `trailofbits/coop`).
//! The [`RepoSlug`] newtype is the only constructor; once a value is held as
//! `RepoSlug`, downstream code knows it is well-formed without re-validating.

use std::fmt;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::naming::is_safe_name_char;

/// A validated `owner/repo` GitHub slug.
///
/// The inner string is module-private — every value must come from
/// [`RepoSlug::new`] (or [`FromStr`] / [`Deserialize`], both of which
/// delegate to it). The constructor enforces:
/// - exactly one `/` separator
/// - non-empty owner and repo segments
/// - GitHub-allowed characters only: ASCII alphanumerics plus `-`, `_`, `.`
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoSlug(String);

impl RepoSlug {
    /// Validate `s` and wrap it. Returns an error describing the violation.
    pub fn new(s: &str) -> Result<Self> {
        let (owner, repo) = s
            .split_once('/')
            .with_context(|| format!("Repo slug must be 'owner/repo', got '{s}'"))?;
        if owner.is_empty() || repo.is_empty() {
            bail!("Repo slug 'owner/repo' must have non-empty owner and repo: '{s}'");
        }
        if repo.contains('/') {
            bail!("Repo slug must contain exactly one '/' separator: '{s}'");
        }
        if !is_valid_segment(owner) || !is_valid_segment(repo) {
            bail!(
                "Repo slug '{s}' contains invalid characters \
                 (allowed: a-z, A-Z, 0-9, '-', '_', '.')"
            );
        }
        Ok(Self(s.to_string()))
    }

    /// Parse a slug from a CLI argument, trimming surrounding whitespace first.
    ///
    /// Used as a clap `value_parser` so `--repo`/positional repo args become a
    /// validated [`RepoSlug`] at the parse boundary instead of a raw `String`.
    pub fn parse_cli(s: &str) -> Result<Self> {
        Self::new(s.trim())
    }

    /// Borrow the slug as `&str` (for formatting / passing to `&str` APIs).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Owner segment (left of the `/`).
    pub fn owner(&self) -> &str {
        // Constructor guarantees a single `/` with non-empty segments.
        self.0.split_once('/').map_or(&self.0, |(o, _)| o)
    }

    /// Repository segment (right of the `/`).
    pub fn repo(&self) -> &str {
        self.0.split_once('/').map_or("", |(_, r)| r)
    }
}

/// GitHub usernames and repo names allow ASCII letters, digits, `-`, `_`, `.`.
/// Leading/trailing `.` or `-` is unusual but accepted by the API; we don't
/// gate that here. Shares the safe-name character class with [`crate::naming`].
fn is_valid_segment(s: &str) -> bool {
    s.chars().all(is_safe_name_char)
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RepoSlug {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for RepoSlug {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepoSlug {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
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
/// Returns `None` for non-GitHub URLs, malformed input, or paths whose
/// owner/repo segments are not valid GitHub identifiers.
pub fn parse_repo_slug_from_url(url: &str) -> Option<RepoSlug> {
    let trimmed = url.trim();
    for prefix in GITHUB_URL_PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return canonicalize(rest);
        }
    }
    None
}

fn canonicalize(path: &str) -> Option<RepoSlug> {
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    // Reject anything past `owner/repo` (e.g. `owner/repo/pulls`).
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    RepoSlug::new(&format!("{owner}/{repo}")).ok()
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
pub fn detect_workspace_repo(git_dir: &Path) -> Result<Option<RepoSlug>> {
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

/// A clone URL passed to `coop up --git-repo`, paired with the
/// `owner/repo` slug derived from it once at construction.
///
/// The slug is computed by [`parse_repo_slug_from_url`] exactly once —
/// when the value is built or deserialized — rather than re-parsed on
/// every read. `slug` is `None` for clone URLs that are not GitHub
/// `owner/repo` URLs (e.g. another host, or a local path); such repos
/// still clone, they just carry no slug for PAT routing.
///
/// On disk the value is a plain URL string (the slug is derived, never
/// persisted), so `workspace.json` files round-trip unchanged.
#[derive(Debug, Clone)]
pub struct GitRepoUrl {
    url: String,
    slug: Option<RepoSlug>,
}

impl GitRepoUrl {
    /// Build from a clone URL, deriving the slug once.
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        let slug = parse_repo_slug_from_url(&url);
        Self { url, slug }
    }

    /// The original clone URL.
    pub fn as_str(&self) -> &str {
        &self.url
    }

    /// The `owner/repo` slug derived from the URL, if it is a GitHub URL.
    pub fn slug(&self) -> Option<&RepoSlug> {
        self.slug.as_ref()
    }
}

/// Two `GitRepoUrl`s are equal when their clone URLs match; the slug is
/// derived and never diverges for a given URL.
impl PartialEq for GitRepoUrl {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}

impl Eq for GitRepoUrl {}

impl fmt::Display for GitRepoUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url)
    }
}

impl Serialize for GitRepoUrl {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.url)
    }
}

impl<'de> Deserialize<'de> for GitRepoUrl {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let url = String::deserialize(deserializer)?;
        Ok(Self::new(url))
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::new(s).unwrap()
    }

    #[test]
    fn parses_https_url() {
        assert_eq!(
            parse_repo_slug_from_url("https://github.com/trailofbits/coop"),
            Some(slug("trailofbits/coop"))
        );
    }

    #[test]
    fn parses_https_url_with_git_suffix() {
        assert_eq!(
            parse_repo_slug_from_url("https://github.com/trailofbits/coop.git"),
            Some(slug("trailofbits/coop"))
        );
    }

    #[test]
    fn parses_https_url_with_trailing_slash() {
        assert_eq!(
            parse_repo_slug_from_url("https://github.com/trailofbits/coop/"),
            Some(slug("trailofbits/coop"))
        );
    }

    #[test]
    fn parses_ssh_url() {
        assert_eq!(
            parse_repo_slug_from_url("git@github.com:trailofbits/coop"),
            Some(slug("trailofbits/coop"))
        );
    }

    #[test]
    fn parses_ssh_url_with_git_suffix() {
        assert_eq!(
            parse_repo_slug_from_url("git@github.com:trailofbits/coop.git"),
            Some(slug("trailofbits/coop"))
        );
    }

    #[test]
    fn parses_ssh_url_with_scheme() {
        assert_eq!(
            parse_repo_slug_from_url("ssh://git@github.com/trailofbits/coop.git"),
            Some(slug("trailofbits/coop"))
        );
    }

    #[test]
    fn parses_http_url() {
        assert_eq!(
            parse_repo_slug_from_url("http://github.com/trailofbits/coop"),
            Some(slug("trailofbits/coop"))
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
    fn rejects_url_with_bad_chars() {
        // Canonicalisation strips trailing slashes and `.git`, but the
        // segment-level character check still rejects invalid characters.
        assert_eq!(
            parse_repo_slug_from_url("https://github.com/owner!/repo"),
            None
        );
    }

    #[test]
    fn accepts_canonical_slug() {
        assert!(RepoSlug::new("trailofbits/coop").is_ok());
        assert!(RepoSlug::new("user-name/repo.name_v2").is_ok());
    }

    #[test]
    fn rejects_missing_slash() {
        assert!(RepoSlug::new("trailofbits").is_err());
    }

    #[test]
    fn rejects_empty_segment() {
        assert!(RepoSlug::new("/coop").is_err());
        assert!(RepoSlug::new("trailofbits/").is_err());
    }

    #[test]
    fn rejects_extra_slash() {
        assert!(RepoSlug::new("a/b/c").is_err());
    }

    #[test]
    fn rejects_bad_chars() {
        assert!(RepoSlug::new("owner!/repo").is_err());
        assert!(RepoSlug::new("owner repo/x").is_err());
    }

    #[test]
    fn accessors_split_at_slash() {
        let s = slug("trailofbits/coop");
        assert_eq!(s.owner(), "trailofbits");
        assert_eq!(s.repo(), "coop");
        assert_eq!(s.as_str(), "trailofbits/coop");
    }

    #[test]
    fn display_matches_inner() {
        assert_eq!(format!("{}", slug("a/b")), "a/b");
    }

    #[test]
    fn from_str_round_trips() {
        let s: RepoSlug = "trailofbits/coop".parse().unwrap();
        assert_eq!(s.as_str(), "trailofbits/coop");
    }

    #[test]
    fn parse_cli_trims_surrounding_whitespace() {
        let s = RepoSlug::parse_cli("  trailofbits/coop\n").unwrap();
        assert_eq!(s.as_str(), "trailofbits/coop");
    }

    #[test]
    fn parse_cli_rejects_invalid_slug() {
        assert!(RepoSlug::parse_cli("not-a-slug").is_err());
    }

    #[test]
    fn deserialize_accepts_valid_slug() {
        let s: RepoSlug = serde_json::from_str("\"a/b\"").unwrap();
        assert_eq!(s.as_str(), "a/b");
    }

    #[test]
    fn deserialize_rejects_invalid_slug() {
        let err = serde_json::from_str::<RepoSlug>("\"not-a-slug\"").unwrap_err();
        assert!(
            err.to_string().contains("owner/repo"),
            "expected owner/repo error, got: {err}"
        );
    }

    #[test]
    fn serialize_round_trips() {
        let s = slug("a/b");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"a/b\"");
    }

    #[test]
    fn git_repo_url_derives_slug_for_github_url() {
        let url = GitRepoUrl::new("https://github.com/trailofbits/coop.git");
        assert_eq!(url.as_str(), "https://github.com/trailofbits/coop.git");
        assert_eq!(url.slug(), Some(&slug("trailofbits/coop")));
    }

    #[test]
    fn git_repo_url_has_no_slug_for_non_github_url() {
        let url = GitRepoUrl::new("https://gitlab.com/group/project.git");
        assert_eq!(url.slug(), None);
        // A repo with no slug still round-trips its clone URL.
        assert_eq!(url.as_str(), "https://gitlab.com/group/project.git");
    }

    #[test]
    fn git_repo_url_serializes_as_bare_string() {
        let url = GitRepoUrl::new("https://github.com/a/b.git");
        let json = serde_json::to_string(&url).unwrap();
        assert_eq!(json, "\"https://github.com/a/b.git\"");
    }

    #[test]
    fn git_repo_url_deserializes_and_reparses_slug() {
        let url: GitRepoUrl = serde_json::from_str("\"https://github.com/a/b\"").unwrap();
        assert_eq!(url.as_str(), "https://github.com/a/b");
        assert_eq!(url.slug(), Some(&slug("a/b")));
    }

    #[test]
    fn git_repo_url_equality_is_by_url() {
        assert_eq!(
            GitRepoUrl::new("https://github.com/a/b"),
            GitRepoUrl::new("https://github.com/a/b")
        );
        assert_ne!(
            GitRepoUrl::new("https://github.com/a/b"),
            GitRepoUrl::new("https://github.com/a/c")
        );
    }
}
