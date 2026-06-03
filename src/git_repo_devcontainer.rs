//! Host-side discovery of `.devcontainer/devcontainer.json` for `--git-repo`.
//!
//! The repository has not been cloned into the guest when start-time
//! devcontainer translation runs, so local filesystem discovery cannot see the
//! file. This module keeps the remote path intentionally narrow: GitHub repo
//! URLs only, one contents API request, and no cache.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use crate::cmd::Cmd;
use crate::config::{GitHubAuth, resolve_cmd_value};
use crate::devcontainer::DEFAULT_DEVCONTAINER_REL_PATH;
use crate::github_repo::{RepoSlug, parse_repo_slug_from_url};

/// A devcontainer file discovered from a repository before the clone exists.
#[derive(Debug, Clone)]
pub struct GitRepoDevcontainer {
    /// User-facing source path used in prompts and reports.
    pub display_path: PathBuf,
    /// File contents returned by the provider.
    pub contents: String,
}

/// Best-effort discovery for `--git-repo`.
///
/// Returns `Ok(None)` for unsupported repository URLs, missing files, missing
/// credentials for private repositories, rate limits, or provider errors. Those
/// cases should not prevent the normal VM startup path from cloning the repo.
pub fn discover(repo_url: &str, auth: Option<&GitHubAuth>) -> Result<Option<GitRepoDevcontainer>> {
    let Some(slug) = parse_repo_slug_from_url(repo_url) else {
        return Ok(None);
    };
    let url = github_contents_url(&slug);
    let token = discovery_token(auth, &slug)?;
    match curl_github_contents(&url, token.as_deref()) {
        Ok((200, body)) => Ok(Some(GitRepoDevcontainer {
            display_path: github_display_path(&slug),
            contents: body,
        })),
        Ok((404, _)) => {
            tracing::debug!("No remote devcontainer.json found for {slug}");
            Ok(None)
        }
        Ok((status, _)) => {
            tracing::debug!(
                "GitHub devcontainer discovery for {slug} returned HTTP {status}; skipping"
            );
            Ok(None)
        }
        Err(e) => {
            tracing::debug!("GitHub devcontainer discovery for {slug} failed: {e:#}");
            Ok(None)
        }
    }
}

fn github_contents_url(slug: &RepoSlug) -> String {
    format!("https://api.github.com/repos/{slug}/contents/{DEFAULT_DEVCONTAINER_REL_PATH}")
}

fn github_display_path(slug: &RepoSlug) -> PathBuf {
    PathBuf::from(format!("github.com/{slug}/{DEFAULT_DEVCONTAINER_REL_PATH}"))
}

fn discovery_token(auth: Option<&GitHubAuth>, slug: &RepoSlug) -> Result<Option<String>> {
    if let Some(entry) = auth.and_then(|a| a.pat_entry(slug)) {
        let token = resolve_cmd_value(entry.token.expose())
            .with_context(|| format!("Failed to resolve token for [github.pat.\"{slug}\"]"))?;
        return Ok(normalize_token(&token));
    }
    Ok(select_host_token(
        gh_auth_token().as_deref(),
        std::env::var("GITHUB_TOKEN").ok().as_deref(),
    ))
}

fn curl_github_contents(url: &str, token: Option<&str>) -> Result<(u16, String)> {
    let mut cmd = Cmd::new("curl")
        .arg("-sSL")
        .arg("--connect-timeout")
        .arg("5")
        .arg("--max-time")
        .arg("15")
        .arg("-H")
        .arg("User-Agent: coop")
        .arg("-H")
        .arg("Accept: application/vnd.github.raw+json")
        .arg("-w")
        .arg("\n%{http_code}");
    if let Some(token) = token {
        cmd = cmd
            .arg("-H")
            .arg("@-")
            .stdin_input(format!("Authorization: token {token}\n"));
    }
    let out = cmd
        .arg(url)
        .capture()
        .with_context(|| format!("curl GET {url} failed"))?;
    parse_curl_status_body(&out)
}

fn parse_curl_status_body(out: &str) -> Result<(u16, String)> {
    let (body, status_str) = out.rsplit_once('\n').unwrap_or(("", out.trim()));
    let status: u16 = status_str
        .trim()
        .parse()
        .with_context(|| format!("Failed to parse HTTP status from curl output: '{status_str}'"))?;
    Ok((status, body.to_string()))
}

fn gh_auth_token() -> Option<String> {
    Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| normalize_token(&s))
}

fn select_host_token(gh: Option<&str>, env: Option<&str>) -> Option<String> {
    gh.and_then(normalize_token)
        .or_else(|| env.and_then(normalize_token))
}

fn normalize_token(token: &str) -> Option<String> {
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests use expect for clear assertions")]
mod tests {
    use super::*;

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::new(s).expect("valid slug")
    }

    #[test]
    fn github_contents_url_targets_default_branch_contents_api() {
        assert_eq!(
            github_contents_url(&slug("owner/repo")),
            "https://api.github.com/repos/owner/repo/contents/.devcontainer/devcontainer.json"
        );
    }

    #[test]
    fn github_display_path_is_user_facing() {
        assert_eq!(
            github_display_path(&slug("owner/repo")),
            PathBuf::from("github.com/owner/repo/.devcontainer/devcontainer.json")
        );
    }

    #[test]
    fn parse_curl_status_body_splits_body_and_status() {
        let (status, body) = parse_curl_status_body("{\"name\":\"x\"}\n200").expect("parsed");
        assert_eq!(status, 200);
        assert_eq!(body, "{\"name\":\"x\"}");
    }

    #[test]
    fn select_host_token_prefers_gh_then_env_and_trims() {
        assert_eq!(
            select_host_token(Some(" ghp_abc \n"), Some("ghp_env")),
            Some("ghp_abc".to_string())
        );
        assert_eq!(
            select_host_token(Some("   "), Some(" ghp_env\n")),
            Some("ghp_env".to_string())
        );
        assert_eq!(select_host_token(Some(""), Some(" ")), None);
    }
}
