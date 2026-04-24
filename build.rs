//! Build script: embed git metadata for version reporting and dev-build detection.
//!
//! Emits four compile-time env vars consumed via `env!()` in the crate:
//! - `COOP_GIT_SHA` — short commit sha, or "unknown" if no git tree.
//! - `COOP_GIT_DIRTY` — "true" if `git status --porcelain` is non-empty, else "false".
//! - `COOP_BUILD_KIND` — "release" iff HEAD is tagged with `v{CARGO_PKG_VERSION}` and the
//!   tree is clean, otherwise "dev".
//! - `COOP_VERSION_STR` — pre-formatted version string for clap's `--version` output.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Invalidate the build script (and thus the embedded version) whenever any of git's
    // state-tracking files change. We resolve the real gitdir first so this also works
    // inside worktrees, where `.git` is a file pointing at `<maindir>/.git/worktrees/<n>`.
    for path in git_rerun_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    // Build-time test hook: allows the integration test to produce a "release" binary from
    // an untagged branch. Runtime behaviour is unaffected — the override only influences
    // what build.rs bakes into `COOP_BUILD_KIND`.
    println!("cargo:rerun-if-env-changed=COOP_FORCE_BUILD_KIND");

    let sha = git_short_sha().unwrap_or_else(|| "unknown".to_string());
    let dirty = git_is_dirty();
    let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let computed = build_kind(&sha, dirty, &pkg_version);
    let kind = match env::var("COOP_FORCE_BUILD_KIND").as_deref() {
        Ok("release") => "release",
        Ok("dev") => "dev",
        Ok(_) | Err(_) => computed,
    };
    let version_str = format_version(&pkg_version, &sha, dirty, kind);

    println!("cargo:rustc-env=COOP_GIT_SHA={sha}");
    println!(
        "cargo:rustc-env=COOP_GIT_DIRTY={}",
        if dirty { "true" } else { "false" }
    );
    println!("cargo:rustc-env=COOP_BUILD_KIND={kind}");
    println!("cargo:rustc-env=COOP_VERSION_STR={version_str}");
}

/// Files whose mtimes should invalidate the build script. Resolves `.git` to the real
/// gitdir when the repo is a worktree (`.git` is a `gitdir: <path>` file, not a directory).
fn git_rerun_paths() -> Vec<PathBuf> {
    let gitdir = resolve_gitdir().unwrap_or_else(|| PathBuf::from(".git"));
    vec![gitdir.join("HEAD"), gitdir.join("index")]
}

fn resolve_gitdir() -> Option<PathBuf> {
    let dot_git = PathBuf::from(".git");
    let meta = fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    // Worktree: `.git` is a file containing `gitdir: <path>`.
    let text = fs::read_to_string(&dot_git).ok()?;
    let target = text.lines().find_map(|l| l.strip_prefix("gitdir: "))?;
    Some(PathBuf::from(target.trim()))
}

fn git_short_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn git_is_dirty() -> bool {
    let Ok(out) = Command::new("git").args(["status", "--porcelain"]).output() else {
        return false;
    };
    out.status.success() && !out.stdout.is_empty()
}

fn build_kind(sha: &str, dirty: bool, pkg_version: &str) -> &'static str {
    if sha == "unknown" || dirty {
        return "dev";
    }
    let expected_tag = format!("v{pkg_version}");
    let Ok(out) = Command::new("git")
        .args(["describe", "--exact-match", "--tags", "HEAD"])
        .output()
    else {
        return "dev";
    };
    if !out.status.success() {
        return "dev";
    }
    match String::from_utf8(out.stdout) {
        Ok(tag) if tag.trim() == expected_tag => "release",
        Ok(_) | Err(_) => "dev",
    }
}

fn format_version(pkg_version: &str, sha: &str, dirty: bool, kind: &str) -> String {
    let dev_suffix = if kind == "dev" { "-dev" } else { "" };
    let dirty_suffix = if dirty { "+dirty" } else { "" };
    format!("{pkg_version}{dev_suffix} ({sha}{dirty_suffix})")
}
