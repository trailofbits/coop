use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

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
