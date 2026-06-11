//! Direction-typed path newtypes that flow between host and guest.
//!
//! `GuestPath` and `HostPath` are paired so that scp/rsync call sites
//! distinguish source from destination by type rather than by argument
//! order. `GuestPath` additionally carries the SFTP-friendly `./...`
//! vs. absolute `/...` distinction that prevents the OpenSSH 9+
//! tilde-expansion gotcha (see CLAUDE.md).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

// ── Guest path ────────────────────────────────────────────────

/// Path inside the guest VM. Prevents confusion between host
/// `PathBuf` and guest path strings (which use Linux conventions
/// regardless of the host OS).
///
/// The permissive [`Self::new`] constructor accepts any string;
/// callers that require an absolute guest path (e.g. mount targets,
/// workspace roots) construct via [`Self::absolute`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GuestPath(String);

impl GuestPath {
    /// Permissive constructor. Accepts any guest path, including
    /// relative SFTP-style `./foo` used by scp targets in the home
    /// directory.
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// Construct a `GuestPath` that is required to be absolute
    /// (starts with `/`). Used for fields that downstream code reads
    /// as already-absolute (mount targets, workspace roots) so the
    /// invariant lives at construction rather than at every use.
    pub fn absolute(path: impl Into<String>) -> Result<Self> {
        let path = path.into();
        if !path.starts_with('/') {
            bail!("Guest path must be absolute: '{path}'");
        }
        Ok(Self(path))
    }
}

impl std::fmt::Display for GuestPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for GuestPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ── Host path ─────────────────────────────────────────────────

/// Path on the host. Pairs with [`GuestPath`] so scp call sites
/// distinguish source and destination by type rather than by
/// argument order. Carries no invariant beyond direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPath(PathBuf);

impl HostPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl From<PathBuf> for HostPath {
    fn from(p: PathBuf) -> Self {
        Self(p)
    }
}

impl From<&Path> for HostPath {
    fn from(p: &Path) -> Self {
        Self(p.to_path_buf())
    }
}

impl AsRef<Path> for HostPath {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn absolute_accepts_leading_slash() {
        let p = GuestPath::absolute("/workspace").unwrap();
        assert_eq!(p.to_string(), "/workspace");
    }

    #[test]
    fn absolute_rejects_relative() {
        let err = GuestPath::absolute("./foo").unwrap_err().to_string();
        assert!(err.contains("must be absolute"), "{err}");
    }

    #[test]
    fn absolute_rejects_empty() {
        assert!(GuestPath::absolute("").is_err());
    }

    #[test]
    fn new_accepts_relative_dot_slash() {
        // Permissive constructor: `./...` is intentional (SFTP tilde
        // expansion gotcha, see CLAUDE.md).
        let p = GuestPath::new("./.claude");
        assert_eq!(p.to_string(), "./.claude");
    }

    #[test]
    fn serde_round_trip_is_transparent() {
        let p = GuestPath::new("/x/y");
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"/x/y\"");
        let back: GuestPath = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
