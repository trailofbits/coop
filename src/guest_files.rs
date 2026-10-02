//! Explicit host files copied into private, writable guest locations at boot.

use std::ffi::CString;
use std::fs::{self, File};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::backend::SshTarget;
use crate::config::{ConfigPath, Mount};
use crate::guest::GuestUser;
use crate::paths::{GuestPath, HostPath};
use crate::remote_command::RemoteCommand;

#[mutants::skip] // OR/XOR are equivalent for disjoint flags; descriptor and symlink tests check behavior.
fn source_open_flags() -> libc::c_int {
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC
}

/// An explicit host source and its destination inside each guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestFile {
    pub source: ConfigPath,
    pub destination: GuestDestination,
}

/// A normalized absolute guest path, or a path relative to the guest's home.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GuestDestination(String);

impl TryFrom<String> for GuestDestination {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        let path = value.strip_prefix("~/").or_else(|| value.strip_prefix('/'));
        let Some(path) = path else {
            bail!("guest_files destination must start with / or ~/: {value:?}");
        };
        ensure!(
            !value.chars().any(char::is_control)
                && path
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "guest_files destination must name a normalized path below / or ~/: {value:?}"
        );
        Ok(Self(value))
    }
}

impl From<GuestDestination> for String {
    fn from(value: GuestDestination) -> Self {
        value.0
    }
}

impl GuestDestination {
    fn resolve(&self, user: &GuestUser) -> GuestPath {
        match self.0.strip_prefix("~/") {
            Some(relative) => GuestPath::new(format!("{}/{relative}", user.home())),
            None => GuestPath::new(self.0.clone()),
        }
    }
}

pub(crate) struct StagedFiles {
    directory: tempfile::TempDir,
    destinations: Vec<GuestPath>,
}

impl StagedFiles {
    /// Snapshot sources before starting or replacing a VM disk.
    pub(crate) fn prepare(files: &[GuestFile], user: &GuestUser, mounts: &[Mount]) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("coop-guest-files-")
            .tempdir()?;
        crate::fs_util::private_dir(directory.path())
            .context("Cannot secure guest_files staging directory")?;
        let mut roots = Vec::new();
        let mut destinations = Vec::new();
        for file in files {
            roots.push(file.source.canonicalize().with_context(|| {
                format!(
                    "Cannot resolve guest_files source {}",
                    file.source.display()
                )
            })?);
            destinations.push(file.destination.resolve(user));
        }
        validate_destinations(&destinations, mounts)?;
        validate_staging_directory(&directory.path().canonicalize()?, &roots)?;
        for (index, root) in roots.iter().enumerate() {
            let container = directory.path().join(index.to_string());
            fs::create_dir(&container)?;
            fs::set_permissions(&container, fs::Permissions::from_mode(0o700))?;
            copy_source(root, &container.join("payload"), &roots, &mut Vec::new())
                .with_context(|| format!("Cannot stage guest_files source {}", root.display()))?;
        }
        Ok(Self {
            directory,
            destinations,
        })
    }

    /// Upload private snapshots, then merge them without removing guest-only files.
    pub(crate) fn install(&self, target: &SshTarget) -> Result<()> {
        for (index, destination) in self.destinations.iter().enumerate() {
            crate::signal::check_shutdown()?;
            self.install_one(target, index, destination)?;
        }
        Ok(())
    }

    fn install_one(&self, target: &SshTarget, index: usize, destination: &GuestPath) -> Result<()> {
        let name = self
            .directory
            .path()
            .file_name()
            .context("Missing staging directory name")?;
        let name = name.to_str().context("Non-UTF-8 staging directory name")?;
        let remote = GuestPath::new(format!("/tmp/{name}-{index}"));
        target.exec(
            RemoteCommand::new()
                .literal("mkdir -m 700 -- ")
                .arg(remote.as_ref()),
        )?;
        let result = (|| {
            target.scp_to_recursive(
                &HostPath::new(
                    self.directory
                        .path()
                        .join(index.to_string())
                        .join("payload"),
                ),
                &remote,
            )?;
            target.exec_with_stdin(
                RemoteCommand::new()
                    .literal("bash -s -- ")
                    .arg(remote.as_ref())
                    .literal(" ")
                    .arg(destination.as_ref()),
                include_bytes!("../scripts/guest/copy-files.sh").to_vec(),
            )
        })();
        let cleanup = target.exec(
            RemoteCommand::new()
                .literal("rm -r -- ")
                .arg(remote.as_ref()),
        );
        if let Err(error) = cleanup {
            tracing::warn!("Failed to remove guest_files staging directory: {error}");
        }
        result.with_context(|| format!("Cannot copy guest_files to {destination}"))
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn validate_staging_directory(directory: &Path, roots: &[PathBuf]) -> Result<()> {
    for root in roots {
        ensure!(
            !directory.starts_with(root),
            "guest_files source {} contains the staging directory; set TMPDIR outside your sources",
            root.display()
        );
    }
    Ok(())
}

/// Open a canonical source without following replaced path components.
fn open_source(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() || metadata.is_dir(),
        "guest_files source is not a regular file or directory: {}",
        path.display()
    );
    let mut file = File::open("/")?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = CString::new(name.as_bytes())?;
        let mut flags = source_open_flags();
        if components.peek().is_some() {
            flags |= libc::O_DIRECTORY;
        }
        // SAFETY: file owns a live directory descriptor; name is NUL-terminated.
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd == -1 {
            return Err(std::io::Error::last_os_error())
                .context("Cannot open guest_files source without following symlinks");
        }
        // SAFETY: openat returned a new descriptor, uniquely owned here.
        file = unsafe { File::from_raw_fd(fd) };
    }
    Ok(file)
}

fn validate_destinations(destinations: &[GuestPath], mounts: &[Mount]) -> Result<()> {
    for (index, destination) in destinations.iter().enumerate() {
        let path = Path::new(destination.as_ref());
        for other in &destinations[..index] {
            ensure!(
                !paths_overlap(path, Path::new(other.as_ref())),
                "guest_files destinations overlap: {destination} and {other}"
            );
        }
        for mount in mounts {
            ensure!(
                !paths_overlap(path, Path::new(mount.guest_path.as_ref())),
                "guest_files destination {destination} overlaps mount {}",
                mount.guest_path
            );
        }
    }
    Ok(())
}

fn copy_source(
    source: &Path,
    dest: &Path,
    roots: &[PathBuf],
    ancestors: &mut Vec<PathBuf>,
) -> Result<()> {
    let source = source
        .canonicalize()
        .context("Dangling or inaccessible guest_files source link")?;
    ensure!(
        roots.iter().any(|root| source.starts_with(root)),
        "Source link escapes declared guest_files roots: {}",
        source.display()
    );
    ensure!(
        !ancestors.contains(&source),
        "Source link cycle at {}",
        source.display()
    );
    let input = open_source(&source)?;
    let metadata = input.metadata()?;
    if metadata.is_file() {
        copy_regular_file(input, dest, &metadata)?;
    } else if metadata.is_dir() {
        fs::create_dir(dest)?;
        fs::set_permissions(dest, fs::Permissions::from_mode(0o700))?;
        ancestors.push(source.clone());
        for entry in fs::read_dir(&source)? {
            let entry = entry?;
            copy_source(
                &entry.path(),
                &dest.join(entry.file_name()),
                roots,
                ancestors,
            )?;
        }
        ancestors.pop();
    } else {
        bail!(
            "guest_files source is not a regular file or directory: {}",
            source.display()
        );
    }
    Ok(())
}

fn copy_regular_file(mut input: File, dest: &Path, metadata: &fs::Metadata) -> Result<()> {
    std::io::copy(&mut input, &mut File::create(dest)?)?;
    let mode = if metadata.permissions().mode() & 0o100 == 0 {
        0o600
    } else {
        0o700
    };
    fs::set_permissions(dest, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixtures")]
mod tests {
    use crate::config::{ConfigPath, CoopConfig};
    use crate::guest::GuestUser;
    use crate::guest_files::{
        GuestDestination, GuestFile, StagedFiles, open_source, validate_destinations,
        validate_staging_directory,
    };
    use crate::paths::GuestPath;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    fn mapping(source: &Path, destination: &str) -> GuestFile {
        GuestFile {
            source: ConfigPath::new(source),
            destination: GuestDestination::try_from(destination.to_owned()).unwrap(),
        }
    }

    #[test]
    fn multiple_mappings_round_trip_and_resolve_guest_home() {
        let cfg: CoopConfig = toml::from_str(
            "[[guest_files]]\nsource = '~/.config/agents'\ndestination = '~/.config/agents'\n\
             [[guest_files]]\nsource = '/etc/example'\ndestination = '/tmp/example'\n",
        )
        .unwrap();
        assert_eq!(cfg.guest_files.len(), 2);
        let user = GuestUser::new("developer").unwrap();
        assert_eq!(
            cfg.guest_files[0].destination.resolve(&user).as_ref(),
            "/home/developer/.config/agents"
        );
        assert_eq!(
            cfg.guest_files[1].destination.resolve(&user).as_ref(),
            "/tmp/example"
        );
        assert!(cfg.guest_files[0].source.is_absolute());
        let encoded = toml::to_string(&cfg).unwrap();
        let decoded: CoopConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.guest_files[0].source, cfg.guest_files[0].source);
        assert_eq!(decoded.guest_files[0].destination.0, "~/.config/agents");
        assert!(CoopConfig::default().guest_files.is_empty());
    }

    #[test]
    fn destinations_reject_non_normalized_and_control_paths() {
        for value in [
            "", "/", "~", "~/", "relative", "~other/a", "/a/", "/a//b", "/a/../b", "~/./a",
            "/a\nb", "/a\0b",
        ] {
            assert!(
                GuestDestination::try_from(value.to_owned()).is_err(),
                "{value:?}"
            );
        }
        for value in ["~/a", "/a b/c'd", "/a;b", "/a..b"] {
            assert!(
                GuestDestination::try_from(value.to_owned()).is_ok(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn overlapping_destinations_are_rejected_at_component_boundaries() {
        for pair in [["/a", "/a"], ["/a", "/a/b"], ["/a/b", "/a"]] {
            let paths = pair.map(GuestPath::new);
            assert!(validate_destinations(&paths, &[]).is_err());
        }
        let paths = [GuestPath::new("/a"), GuestPath::new("/ab")];
        assert!(validate_destinations(&paths, &[]).is_ok());
    }

    #[test]
    fn snapshots_copy_dotfiles_materialize_links_and_make_files_writable() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".config"), "original").unwrap();
        fs::set_permissions(
            root.path().join(".config"),
            fs::Permissions::from_mode(0o444),
        )
        .unwrap();
        fs::write(root.path().join("run"), "executable").unwrap();
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o555)).unwrap();
        symlink(".config", root.path().join("alias")).unwrap();
        let staged = StagedFiles::prepare(
            &[mapping(root.path(), "~/config")],
            &GuestUser::default(),
            &[],
        )
        .unwrap();
        let payload = staged.directory.path().join("0/payload");
        fs::set_permissions(
            root.path().join(".config"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::write(root.path().join(".config"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(payload.join("alias")).unwrap(),
            "original"
        );
        assert!(
            !fs::symlink_metadata(payload.join("alias"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            fs::metadata(payload.join(".config"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(payload.join("run"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(payload).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn source_links_can_only_cross_to_another_declared_root() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::write(external.path().join("secret"), "explicit").unwrap();
        symlink(external.path().join("secret"), root.path().join("link")).unwrap();
        let mut files = vec![mapping(root.path(), "~/config")];
        let error = StagedFiles::prepare(&files, &GuestUser::default(), &[])
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("escapes declared"));
        files.push(mapping(&external.path().join("secret"), "~/other"));
        let staged = StagedFiles::prepare(&files, &GuestUser::default(), &[]).unwrap();
        assert_eq!(
            fs::read_to_string(staged.directory.path().join("0/payload/link")).unwrap(),
            "explicit"
        );
        assert_eq!(
            fs::read_to_string(staged.directory.path().join("1/payload")).unwrap(),
            "explicit"
        );
    }

    #[test]
    fn cycles_dangling_links_and_special_files_fail_before_boot() {
        let root = tempfile::tempdir().unwrap();
        let files = [mapping(root.path(), "~/config")];
        symlink(".", root.path().join("cycle")).unwrap();
        let error = StagedFiles::prepare(&files, &GuestUser::default(), &[])
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("cycle"));
        fs::remove_file(root.path().join("cycle")).unwrap();
        symlink("missing", root.path().join("dangling")).unwrap();
        assert!(StagedFiles::prepare(&files, &GuestUser::default(), &[]).is_err());
        fs::remove_file(root.path().join("dangling")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
        let error = StagedFiles::prepare(&files, &GuestUser::default(), &[])
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("not a regular file"));
    }

    #[test]
    fn missing_sources_and_mount_collisions_fail_before_boot() {
        let root = tempfile::tempdir().unwrap();
        let files = [mapping(&root.path().join("missing"), "~/config")];
        assert!(StagedFiles::prepare(&files, &GuestUser::default(), &[]).is_err());
        let mount = crate::config::Mount::from_parts(
            root.path().to_str().unwrap(),
            GuestPath::new("/shared"),
        )
        .unwrap();
        for dest in ["/shared", "/shared/config"] {
            let files = [mapping(root.path(), dest)];
            let error =
                StagedFiles::prepare(&files, &GuestUser::default(), std::slice::from_ref(&mount))
                    .err()
                    .unwrap();
            assert!(format!("{error:#}").contains("overlaps mount"));
        }
    }

    #[test]
    fn staging_directory_cannot_be_inside_a_source() {
        let roots = vec![std::path::PathBuf::from("/sources")];
        assert!(validate_staging_directory(Path::new("/sources/stage"), &roots).is_err());
        assert!(validate_staging_directory(Path::new("/sources"), &roots).is_err());
        assert!(validate_staging_directory(Path::new("/sources-other/stage"), &roots).is_ok());
    }

    #[test]
    fn staging_root_is_private() {
        let staged = StagedFiles::prepare(&[], &GuestUser::default(), &[]).unwrap();
        assert_eq!(
            fs::metadata(staged.directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn source_containing_tmpdir_fails_without_recursing() {
        const MARKER: &str = "COOP_TEST_STAGING_ROOT";
        if let Some(root) = std::env::var_os(MARKER) {
            let files = [mapping(Path::new(&root), "~/copy")];
            let error = StagedFiles::prepare(&files, &GuestUser::default(), &[])
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("contains the staging directory"));
            assert!(fs::read_dir(root).unwrap().next().is_none());
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "guest_files::tests::source_containing_tmpdir_fails_without_recursing",
            ])
            .env("TMPDIR", root.path())
            .env(MARKER, root.path())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn staging_removes_inherited_read_acls() {
        use std::process::Command;
        const MARKER: &str = "COOP_TEST_STAGING_ACL";
        if std::env::var_os(MARKER).is_some() {
            let source = tempfile::tempdir().unwrap();
            fs::write(source.path().join("canary"), "private snapshot").unwrap();
            let staged = StagedFiles::prepare(
                &[mapping(source.path(), "~/copy")],
                &GuestUser::default(),
                &[],
            )
            .unwrap();
            let listing = Command::new("ls")
                .arg("-lde")
                .arg(staged.directory.path())
                .output()
                .unwrap();
            assert!(listing.status.success());
            assert!(!String::from_utf8_lossy(&listing.stdout).contains("allow"));
            assert_eq!(
                fs::read_to_string(staged.directory.path().join("0/payload/canary")).unwrap(),
                "private snapshot"
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        assert!(
            Command::new("chmod")
                .arg("+a")
                .arg("everyone allow read,search,file_inherit,directory_inherit")
                .arg(root.path())
                .status()
                .unwrap()
                .success()
        );
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("guest_files::tests::staging_removes_inherited_read_acls")
            .env("TMPDIR", root.path())
            .env(MARKER, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn replaced_source_components_cannot_redirect_copying() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        fs::create_dir(root.join("source")).unwrap();
        fs::write(root.join("source/file"), "allowed").unwrap();
        fs::write(root.join("secret"), "forbidden").unwrap();
        let canonical = root.join("source/file").canonicalize().unwrap();
        let mut pinned = open_source(&canonical).unwrap();
        fs::rename(&canonical, root.join("original")).unwrap();
        symlink(root.join("secret"), &canonical).unwrap();
        assert!(open_source(&canonical).is_err());
        let mut content = String::new();
        pinned.read_to_string(&mut content).unwrap();
        assert_eq!(content, "allowed");
        fs::remove_file(&canonical).unwrap();
        fs::remove_dir(root.join("source")).unwrap();
        symlink(&root, root.join("source")).unwrap();
        assert!(open_source(&root.join("source/secret")).is_err());
    }

    #[test]
    fn source_descriptors_are_nonblocking_and_close_on_exec() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source"), "data").unwrap();
        let file = open_source(&root.path().join("source").canonicalize().unwrap()).unwrap();
        // SAFETY: these read-only fcntl operations use a live owned descriptor.
        let status = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: file owns the descriptor until after this assertion.
        let descriptor = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(status, -1);
        assert_ne!(descriptor, -1);
        assert_ne!(status & libc::O_NONBLOCK, 0);
        assert_ne!(descriptor & libc::FD_CLOEXEC, 0);
    }

    proptest::proptest! {
        #[test]
        fn guest_destinations_round_trip(components in
            proptest::collection::vec("[a-z][a-z0-9_-]{0,12}", 1..6)) {
            let path = format!("~/{}", components.join("/"));
            let destination = GuestDestination::try_from(path.clone()).unwrap();
            let encoded = serde_json::to_string(&destination).unwrap();
            let decoded: GuestDestination = serde_json::from_str(&encoded).unwrap();
            proptest::prop_assert_eq!(&decoded.0, &path);
            let resolved = decoded.resolve(&GuestUser::default());
            proptest::prop_assert_eq!(resolved.as_ref(),
                format!("/home/ubuntu/{}", components.join("/")));
        }
    }
}
