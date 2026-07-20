//! Version-control-system awareness for workspace sync.
//!
//! coop syncs a workspace between host and guest and runs a handful of
//! VCS-touching operations (dirty checks, metadata protection during
//! transfer). Git was historically the only case, hit ad-hoc. This module
//! centralizes the "which VCS backs this directory, and how do we treat its
//! metadata" decision so both git and [Jujutsu](https://jj-vcs.dev) (`jj`)
//! are handled consistently at every call site.
//!
//! ## The jj layouts
//!
//! - **Colocated** (`jj git init --colocate`, the jj default since 0.30):
//!   both `.jj/` and a top-level `.git/` exist. jj writes `/*` into
//!   `.jj/.gitignore` so colocated git ignores its store.
//! - **Non-colocated** (`jj git init --no-colocate`): only `.jj/` exists;
//!   the backing git repo lives at `.jj/repo/store/git`. There is *no*
//!   top-level `.git/`, so a `.git`-presence check does not see it at all.
//!
//! ## Why transfer protection matters
//!
//! rsync's per-directory `.gitignore` merge honors `.jj/.gitignore`'s `/*`
//! and strips the *entire* jj store during a colocated push/pull, leaving a
//! broken `.jj/` shell (`jj status` → "The repository appears broken").
//! [`rsync_vcs_filters`] force-protects `.jj/` with a `+` rule that must
//! precede that merge, mirroring the existing `.git/` protection.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// The `.git` directory name, as an rsync/tar exclude pattern.
pub const GIT_DIR: &str = ".git/";
/// The `.jj` directory name, as an rsync/tar exclude pattern.
pub const JJ_DIR: &str = ".jj/";

/// The VCS backing a host workspace directory, detected from its layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vcs {
    /// A Jujutsu repo, colocated or not — detected by a `.jj/` directory.
    Jj,
    /// A plain git repo — a `.git/` entry with no `.jj/`.
    Git,
    /// No recognized VCS.
    None,
}

impl Vcs {
    /// Detect the VCS backing `dir`.
    ///
    /// jj wins over git: a colocated jj repo has *both* `.jj/` and `.git/`,
    /// but jj is the source of truth (it drives the git refs), so it is
    /// classified as [`Vcs::Jj`]. Only a `.git/` with no `.jj/` is
    /// [`Vcs::Git`].
    pub fn detect(dir: &Path) -> Self {
        if dir.join(".jj").is_dir() {
            Self::Jj
        } else if dir.join(".git").exists() {
            Self::Git
        } else {
            Self::None
        }
    }

    /// True when `dir` is under any recognized VCS.
    pub fn is_repo(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// rsync `--filter`/`--exclude` args that carry (or drop) VCS metadata.
///
/// When `exclude_git` is false the caller wants history preserved, so both
/// `.git/` and `.jj/` are force-protected with `+` rules. These **must**
/// precede the per-directory `.gitignore` merge the caller appends next:
/// rsync is first-match-wins, and jj writes `/*` into `.jj/.gitignore`, so
/// without the protect rule the merge would strip the whole jj store.
///
/// When `exclude_git` is true both metadata dirs are excluded.
pub fn rsync_vcs_filters(exclude_git: bool) -> Vec<String> {
    if exclude_git {
        vec![
            format!("--exclude={GIT_DIR}"),
            format!("--exclude={JJ_DIR}"),
        ]
    } else {
        // `/.git/***` / `/.jj/***` match the directory itself and everything
        // inside, anchored at the transfer root.
        vec![
            "--filter=+ /.git/***".to_string(),
            "--filter=+ /.jj/***".to_string(),
        ]
    }
}

/// tar `--exclude` args for VCS metadata dirs.
///
/// tar has no per-tree "protect" rule, so this only *excludes* (when
/// `exclude_git` is true). When preserving history it adds nothing: `.jj/`
/// is not in the default excludes, and GNU tar's `--exclude-vcs-ignores`
/// does not strip it (BSD/macOS tar lacks that flag entirely, so it also
/// preserves `.jj/`). A workspace whose *top-level* `.gitignore` lists
/// `.jj/` is the one gap the tar transport cannot protect against; the
/// rsync transport (coop's default) protects it via [`rsync_vcs_filters`].
pub fn tar_vcs_excludes(exclude_git: bool) -> Vec<String> {
    if exclude_git {
        vec![
            format!("--exclude={GIT_DIR}"),
            format!("--exclude={JJ_DIR}"),
        ]
    } else {
        Vec::new()
    }
}

/// Whether the working copy in `dir` has changes coop should refuse to
/// silently overwrite, returning the human-readable change list when dirty.
///
/// Routes on the detected VCS:
/// - [`Vcs::Git`] → `git status --porcelain`.
/// - [`Vcs::Jj`] → `jj diff --name-only` (jj auto-snapshots the working
///   copy into `@`, so "dirty" means `@` differs from its parent; gitignored
///   build noise is excluded, matching the git path's intent).
/// - [`Vcs::None`] → never dirty.
///
/// If `dir` is a jj repo but the `jj` binary is unavailable, the check
/// degrades rather than hard-failing a `coop pull`: a colocated repo falls
/// back to its git check, a non-colocated repo is skipped with a warning.
pub fn working_copy_dirty(dir: &Path) -> Result<Option<String>> {
    match Vcs::detect(dir) {
        Vcs::Jj => jj_working_copy_dirty(dir),
        Vcs::Git => git_working_copy_dirty(dir),
        Vcs::None => Ok(None),
    }
}

fn git_working_copy_dirty(dir: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .context("Failed to check local git status")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(non_empty(stdout.trim()))
}

fn jj_working_copy_dirty(dir: &Path) -> Result<Option<String>> {
    let output = Command::new("jj")
        .arg("--repository")
        .arg(dir)
        .args(["diff", "--name-only"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            Ok(non_empty(stdout.trim()))
        }
        Ok(out) => {
            // jj ran but errored (e.g. a corrupt store). Surface the change
            // list conservatively as "unknown, treat as dirty".
            let stderr = String::from_utf8_lossy(&out.stderr);
            tracing::warn!("jj diff failed in {}: {}", dir.display(), stderr.trim());
            Ok(Some(format!(
                "jj could not determine working-copy status:\n{}",
                stderr.trim()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No jj on the host. Colocated repos can still be checked via git.
            if dir.join(".git").exists() {
                tracing::warn!(
                    "jj not found on host; falling back to git status for colocated repo {}",
                    dir.display()
                );
                git_working_copy_dirty(dir)
            } else {
                tracing::warn!(
                    "jj not found on host; skipping dirty check for non-colocated jj repo {}",
                    dir.display()
                );
                Ok(None)
            }
        }
        Err(e) => Err(anyhow::Error::new(e).context("Failed to run jj diff")),
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn rsync_filters_protect_both_vcs_dirs_when_keeping_history() {
        let f = rsync_vcs_filters(false);
        assert!(f.iter().any(|a| a == "--filter=+ /.git/***"));
        assert!(f.iter().any(|a| a == "--filter=+ /.jj/***"));
        assert!(!f.iter().any(|a| a.starts_with("--exclude=")));
    }

    #[test]
    fn rsync_filters_exclude_both_vcs_dirs_when_dropping_history() {
        let f = rsync_vcs_filters(true);
        assert!(f.iter().any(|a| a == "--exclude=.git/"));
        assert!(f.iter().any(|a| a == "--exclude=.jj/"));
        assert!(!f.iter().any(|a| a.starts_with("--filter=+")));
    }

    #[test]
    fn tar_excludes_only_when_dropping_history() {
        assert!(tar_vcs_excludes(false).is_empty());
        let f = tar_vcs_excludes(true);
        assert!(f.iter().any(|a| a == "--exclude=.git/"));
        assert!(f.iter().any(|a| a == "--exclude=.jj/"));
    }

    #[test]
    fn detect_none_on_plain_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(Vcs::detect(dir.path()), Vcs::None);
        assert!(!Vcs::detect(dir.path()).is_repo());
        assert!(working_copy_dirty(dir.path()).expect("ok").is_none());
    }

    #[test]
    fn detect_prefers_jj_over_git_when_colocated() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("mk .git");
        std::fs::create_dir(dir.path().join(".jj")).expect("mk .jj");
        assert_eq!(Vcs::detect(dir.path()), Vcs::Jj);
    }

    #[test]
    fn detect_git_when_only_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("mk .git");
        assert_eq!(Vcs::detect(dir.path()), Vcs::Git);
    }
}
