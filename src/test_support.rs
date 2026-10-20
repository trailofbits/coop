use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::Path;

/// Create a fixture root whose trust does not depend on the process umask.
pub(crate) fn private_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
}

/// Create every missing fixture directory with private permissions.
pub(crate) fn create_private_dir_all(path: impl AsRef<Path>) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}
