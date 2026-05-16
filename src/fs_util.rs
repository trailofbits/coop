use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

/// Build a writer-unique sibling temp path. Including pid + a nanosecond
/// counter keeps concurrent writers (across threads or processes) from
/// clobbering each other's tmp files before the final rename.
fn unique_tmp_path(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target.file_name().map_or_else(
        || "coop-atomic".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    parent.join(format!(".{stem}.{}.{nanos}.tmp", std::process::id()))
}

/// Write JSON content to a file atomically via temp file + rename.
///
/// Creates a sibling temp file, writes the content, then renames
/// over the target. If the target exists, its permissions are
/// preserved; otherwise defaults to 0o644.
pub fn atomic_write_json(path: &Path, json: &str) -> Result<()> {
    let parent = path
        .parent()
        .context("Cannot determine parent directory for atomic write")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directory {}", parent.display()))?;

    let perms = if path.exists() {
        fs::metadata(path)
            .with_context(|| format!("Failed to read metadata for {}", path.display()))?
            .permissions()
    } else {
        fs::Permissions::from_mode(0o644)
    };

    let tmp_path = unique_tmp_path(path);
    fs::write(&tmp_path, json)
        .with_context(|| format!("Failed to write temp file {}", tmp_path.display()))?;
    fs::set_permissions(&tmp_path, perms)
        .with_context(|| format!("Failed to set permissions on {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to rename {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

/// RAII file lock acquired via `flock(LOCK_EX)`. Blocks until the lock
/// is available; releases on drop.
///
/// Use [`lock_sibling`] to obtain a lock keyed off a target path.
pub struct FileLock {
    _file: File,
}

/// Acquire an exclusive flock on a sibling `.lock` file next to `target`.
///
/// The lock file is created if necessary and lives across calls — its
/// purpose is purely to serialize access to `target`. Releases on drop.
/// Returns an error if the parent directory cannot be created or the
/// lock cannot be acquired.
pub fn lock_sibling(target: &Path) -> Result<FileLock> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let stem = target
        .file_name()
        .map_or_else(|| "coop".to_string(), |n| n.to_string_lossy().into_owned());
    let lock_path = parent.join(format!(".{stem}.lock"));
    let file = File::create(&lock_path)
        .with_context(|| format!("Failed to create lock file {}", lock_path.display()))?;
    // SAFETY: flock is safe on a valid fd. The File owns the fd and
    // outlives this call.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        bail!(
            "Failed to acquire lock on {}: {}",
            lock_path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(FileLock { _file: file })
}

/// Write `content` to `path` atomically with permissions `mode`.
///
/// If the target file already exists with stricter permissions (any
/// bit in `mode` not also set on the existing file), the existing
/// permissions are preserved — we never relax a file's mode.
pub fn atomic_write_with_mode(path: &Path, content: &str, mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .context("Cannot determine parent directory for atomic write")?;
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let perms = if path.exists() {
        let existing = fs::metadata(path)
            .with_context(|| format!("Failed to read metadata for {}", path.display()))?
            .permissions()
            .mode()
            & 0o777;
        // Pick the more restrictive of (existing, requested).
        let combined = existing & mode;
        fs::Permissions::from_mode(combined)
    } else {
        fs::Permissions::from_mode(mode)
    };
    let tmp_path = unique_tmp_path(path);
    fs::write(&tmp_path, content)
        .with_context(|| format!("Failed to write temp file {}", tmp_path.display()))?;
    fs::set_permissions(&tmp_path, perms)
        .with_context(|| format!("Failed to set permissions on {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to rename {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Write content to a file atomically, preserving SSH-appropriate
/// permissions (0o600 default).
pub fn atomic_write_ssh(path: &Path, content: &str) -> Result<()> {
    let perms = if path.exists() {
        fs::metadata(path)
            .context("Failed to read file metadata")?
            .permissions()
    } else {
        fs::Permissions::from_mode(0o600)
    };

    let tmp_path = unique_tmp_path(path);
    fs::write(&tmp_path, content)
        .with_context(|| format!("Failed to write temp file {}", tmp_path.display()))?;
    fs::set_permissions(&tmp_path, perms)
        .with_context(|| format!("Failed to set permissions on {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to rename {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_json_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        atomic_write_json(&path, r#"{"key": "value"}"#).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"key": "value"}"#);
        // No sibling .tmp files left behind
        let siblings: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(siblings.is_empty(), "stray tmp file remains");
    }

    #[test]
    fn unique_tmp_path_differs_per_call() {
        let target = Path::new("/tmp/coop/test.json");
        let a = unique_tmp_path(target);
        std::thread::sleep(std::time::Duration::from_nanos(1));
        let b = unique_tmp_path(target);
        // Same pid but different nanosecond stamps.
        assert_ne!(a, b);
        assert!(a.to_string_lossy().ends_with(".tmp"));
        assert_eq!(a.parent(), target.parent());
    }

    #[test]
    fn atomic_write_json_preserves_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        atomic_write_json(&path, "new").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn atomic_write_json_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("dir").join("test.json");
        atomic_write_json(&path, "{}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn atomic_write_json_overwrites_completely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");

        // Write long content first
        atomic_write_json(&path, &"x".repeat(1000)).unwrap();
        // Overwrite with short content
        atomic_write_json(&path, "{}").unwrap();

        // Must be exactly the short content, not a partial mix
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn atomic_write_json_default_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.json");
        atomic_write_json(&path, "{}").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn atomic_write_ssh_default_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        atomic_write_ssh(&path, "Host *\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn atomic_write_ssh_preserves_existing_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write_ssh(&path, "new").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn atomic_write_no_temp_file_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        atomic_write_json(&path, "{}").unwrap();

        // Verify no stale .tmp sibling
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name(), "test.json");
    }
}
