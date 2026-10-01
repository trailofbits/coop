use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::FileExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::io::{AsRawFd as _, FromRawFd as _, IntoRawFd as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Create and seal a private directory without following path symlinks.
/// Ancestors must be owned by this user or root; writable shared ancestors
/// must have the sticky bit (for example /tmp). Existing private directories
/// are tightened through their open descriptor before being used.
pub fn private_dir(path: &Path) -> Result<()> {
    PrivateDir::create(path).map(|_| ())
}

/// Seal existing private storage without recreating a concurrently removed path.
#[cfg(test)]
pub fn private_existing_dir(path: &Path) -> Result<()> {
    PrivateDir::open_existing(path).map(|_| ())
}

/// Remove a managed file or invalid symlink through its checked parent.
/// Concurrent removal of the parent or child is an ordinary no-op.
pub(crate) fn remove_private_if_exists(path: &Path) -> Result<()> {
    let parent = path.parent().context("Private file has no parent")?;
    let directory = match PrivateDir::open_existing(parent) {
        Ok(directory) => directory,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let name = path.file_name().context("Private file has no name")?;
    match directory.unlink_non_directory(name) {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Read an optional managed state file through its checked parent descriptor.
pub(crate) fn read_optional_private(path: &Path) -> Result<Option<String>> {
    let parent = path.parent().context("Private file has no parent")?;
    let name = path.file_name().context("Private file has no name")?;
    let read = (|| PrivateDir::open_existing(parent)?.read_to_string(name))();
    match read {
        Ok(content) => Ok(Some(content)),
        Err(error)
            if error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// A checked private directory whose descriptor pins the directory used by
/// subsequent child operations. Names passed to its methods are one component.
pub struct PrivateDir {
    file: File,
}

impl PrivateDir {
    pub fn create(path: &Path) -> Result<Self> {
        seal_private_dir(path, MissingDirectory::Create)
    }

    pub fn open_existing(path: &Path) -> Result<Self> {
        seal_private_dir(path, MissingDirectory::Reject)
    }

    pub fn create_child(&self, name: &OsStr) -> Result<Self> {
        self.open_child(name, MissingDirectory::Create)
    }

    pub fn child(&self, name: &OsStr) -> Result<Self> {
        self.open_child(name, MissingDirectory::Reject)
    }

    fn open_child(&self, name: &OsStr, missing: MissingDirectory) -> Result<Self> {
        let name = child_name(name)?;
        let file = match open_directory_at(&self.file, &name, SymlinkPolicy::Reject) {
            Ok(file) => file,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && matches!(missing, MissingDirectory::Create) =>
            {
                create_private_directory_at(&self.file, &name)?;
                open_directory_at(&self.file, &name, SymlinkPolicy::Reject)?
            }
            Err(error) => return Err(error.into()),
        };
        check_private_directory(&file)?;
        Ok(Self { file })
    }

    /// Open, validate, and seal a user-owned regular file through this directory.
    pub fn open_regular(&self, name: &OsStr) -> Result<File> {
        let file = self.open_checked_regular(name, libc::O_RDONLY)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        clear_private_acl(&file)?;
        Ok(file)
    }

    pub fn open_regular_with_mode(&self, name: &OsStr, maximum_mode: u32) -> Result<File> {
        let file = self.open_checked_regular(name, libc::O_RDONLY)?;
        let existing = file.metadata()?.mode() & 0o777;
        file.set_permissions(fs::Permissions::from_mode(existing & maximum_mode))?;
        clear_private_acl(&file)?;
        Ok(file)
    }

    pub fn open_regular_for_update(&self, name: &OsStr) -> Result<File> {
        let checked = self.open_regular(name)?;
        let writable = self.open_checked_regular(name, libc::O_RDWR)?;
        let original = checked.metadata()?;
        let reopened = writable.metadata()?;
        if original.dev() != reopened.dev() || original.ino() != reopened.ino() {
            bail!("Private file changed while opening for update");
        }
        Ok(writable)
    }

    fn open_checked_regular(&self, name: &OsStr, access: libc::c_int) -> Result<File> {
        let name = child_name(name)?;
        let file = open_file_at(
            &self.file,
            &name,
            access | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )?;
        check_user_regular(&file)?;
        Ok(file)
    }

    pub fn read_to_string(&self, name: &OsStr) -> Result<String> {
        let mut contents = String::new();
        self.open_regular(name)?.read_to_string(&mut contents)?;
        Ok(contents)
    }

    /// Open or create a stable lock inode without truncating it.
    pub fn open_or_create_lock(&self, name: &OsStr) -> Result<File> {
        let name = child_name(name)?;
        let file = open_file_at(
            &self.file,
            &name,
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )?;
        check_user_regular(&file)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        clear_private_acl(&file)?;
        Ok(file)
    }

    /// Remove a regular child from the pinned directory. This assumes the
    /// private namespace remains under the caller's control between checks.
    pub fn remove_child(&self, name: &OsStr) -> Result<()> {
        self.open_regular(name)?;
        unlink_child_at(&self.file, &child_name(name)?)?;
        Ok(())
    }

    /// Unlink an entry without following it. This also permits removing a
    /// rejected symlink so managed state can recover from an invalid entry.
    pub fn unlink_non_directory(&self, name: &OsStr) -> Result<()> {
        if self.entry_type(name)? == PrivateEntryType::Directory {
            bail!("Cannot unlink a private directory through a file operation");
        }
        unlink_child_at(&self.file, &child_name(name)?)?;
        Ok(())
    }

    /// Rename a regular child within the pinned private directory.
    pub fn rename_child(&self, source: &OsStr, target: &OsStr) -> Result<()> {
        self.open_regular(source)?;
        if let Err(error) = self.open_regular(target)
            && error
                .downcast_ref::<std::io::Error>()
                .is_none_or(|io| io.kind() != std::io::ErrorKind::NotFound)
        {
            return Err(error);
        }
        rename_child_at(&self.file, &child_name(source)?, &child_name(target)?)?;
        Ok(())
    }

    /// Atomically replace a name in this private directory. This relies on
    /// exclusive control of the directory namespace: renameat cannot compare
    /// the destination inode with the one inspected for permissions.
    pub fn write_atomic_private(&self, name: &OsStr, content: &[u8], mode: u32) -> Result<()> {
        let mut source = content;
        self.atomic_with_writer(name, mode, |temporary| {
            std::io::copy(&mut source, temporary)?;
            Ok(())
        })
    }

    /// Copy an opened disk while retaining large holes in sparse images.
    pub fn copy_atomic_sparse(&self, name: &OsStr, source: &File, mode: u32) -> Result<()> {
        self.atomic_with_writer(name, mode, |temporary| copy_sparse_file(source, temporary))
    }

    fn atomic_with_writer(
        &self,
        name: &OsStr,
        mode: u32,
        write: impl FnOnce(&mut File) -> Result<()>,
    ) -> Result<()> {
        let target = child_name(name)?;
        let mode = match self.open_checked_regular(name, libc::O_RDONLY) {
            Ok(file) => file.metadata()?.mode() & mode,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                mode
            }
            Err(error) => return Err(error),
        };
        let (temp_name, mut temp_file) = self.create_temporary()?;
        let result = (|| {
            clear_private_acl(&temp_file)?;
            write(&mut temp_file)?;
            temp_file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
            rename_child_at(&self.file, &temp_name, &target)?;
            Ok(())
        })();
        let _ = unlink_child_at(&self.file, &temp_name);
        result
    }

    fn create_temporary(&self) -> Result<(CString, File)> {
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        for _ in 0..16 {
            let index = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(format!(".coop-{}-{index}.tmp", std::process::id()))?;
            match open_file_at(
                &self.file,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            ) {
                Ok(file) => return Ok((name, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error.into()),
            }
        }
        bail!("Cannot allocate a private temporary file")
    }

    pub fn entry_type(&self, name: &OsStr) -> Result<PrivateEntryType> {
        let stat = stat_child_at(&self.file, &child_name(name)?)?;
        Ok(match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => PrivateEntryType::Directory,
            libc::S_IFREG => PrivateEntryType::Regular,
            libc::S_IFLNK => PrivateEntryType::Symlink,
            _ => PrivateEntryType::Other,
        })
    }

    pub(crate) fn entry_stat(&self, name: &OsStr) -> Result<libc::stat> {
        Ok(stat_child_at(&self.file, &child_name(name)?)?)
    }

    pub fn entries(&self) -> Result<Vec<OsString>> {
        // Opening "." creates an independent directory offset; dup/try_clone
        // would share the offset with other enumerations of this handle.
        let cloned = open_directory_at(&self.file, c".", SymlinkPolicy::Reject)?;
        // SAFETY: fdopendir takes ownership of a valid duplicated descriptor.
        let directory = unsafe { libc::fdopendir(cloned.as_raw_fd()) };
        if directory.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let _ = cloned.into_raw_fd();
        let directory = DirectoryStream(directory);
        let mut names = Vec::new();
        loop {
            // SAFETY: errno is thread local and this platform function returns
            // its writable address. Clearing it distinguishes EOF from error.
            unsafe { *errno_location() = 0 };
            // SAFETY: directory owns a live DIR stream; returned entry remains
            // valid until the next readdir call.
            let entry = unsafe { libc::readdir(directory.0) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(0) {
                    break;
                }
                return Err(error.into());
            }
            // SAFETY: d_name is a NUL-terminated string inside the live entry.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                names.push(OsString::from_vec(name.to_bytes().to_vec()));
            }
        }
        Ok(names)
    }

    pub fn read_link(&self, name: &OsStr) -> Result<std::path::PathBuf> {
        let name = child_name(name)?;
        if self.entry_type(OsStr::from_bytes(name.as_bytes()))? != PrivateEntryType::Symlink {
            bail!("Private child is not a symlink");
        }
        let mut buffer = vec![0; 256];
        loop {
            // SAFETY: the directory descriptor and NUL-terminated name are valid;
            // buffer is writable for exactly its reported length.
            let count = unsafe {
                libc::readlinkat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if count.cast_unsigned() < buffer.len() {
                buffer.truncate(count.cast_unsigned());
                return Ok(OsString::from_vec(buffer).into());
            }
            buffer.resize(buffer.len() * 2, 0);
        }
    }
}

/// Copy file data through pinned descriptors. Entirely zero chunks become
/// holes even on filesystems that cannot report `SEEK_DATA`/`SEEK_HOLE`. This is
/// a bounded-memory fallback for raw VM images when reflinks are unavailable.
pub(crate) fn copy_sparse_file(source: &File, target: &File) -> Result<()> {
    let length = source.metadata()?.len();
    target.set_len(0)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut offset = 0_u64;
    while offset < length {
        let remaining = usize::try_from((length - offset).min(buffer.len() as u64))?;
        source.read_exact_at(&mut buffer[..remaining], offset)?;
        if buffer[..remaining].iter().any(|byte| *byte != 0) {
            target.write_all_at(&buffer[..remaining], offset)?;
        }
        offset += remaining as u64;
    }
    target.set_len(length)?;
    Ok(())
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: fdopendir returned this owned DIR stream.
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(target_os = "linux")]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: libc returns a valid thread-local errno address.
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: libc returns a valid thread-local errno address.
    unsafe { libc::__error() }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivateEntryType {
    Directory,
    Regular,
    Symlink,
    Other,
}

fn child_name(name: &OsStr) -> Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        bail!("Private child name must be one component");
    }
    Ok(CString::new(bytes)?)
}

#[derive(Clone, Copy)]
enum MissingDirectory {
    Create,
    Reject,
}

fn seal_private_dir(path: &Path, missing: MissingDirectory) -> Result<PrivateDir> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute == Path::new("/") {
        bail!("The filesystem root cannot be private storage");
    }
    let mut directory = File::open("/")?;
    check_ancestor(&directory)?;
    let components: Vec<_> = absolute.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::ParentDir) {
                bail!(
                    "Private storage path cannot contain '..': {}",
                    path.display()
                );
            }
            continue;
        };
        let name = CString::new(name.as_bytes())?;
        let final_component = index + 1 == components.len();
        let mut opened = open_directory_at(&directory, &name, SymlinkPolicy::Reject);
        if opened.is_err() && !final_component && root_owned_symlink_at(&directory, &name) {
            opened = open_directory_at(&directory, &name, SymlinkPolicy::TrustedAncestor);
        }
        let (next, creation) = match opened {
            Ok(next) => (next, DirectoryCreation::Existing),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && matches!(missing, MissingDirectory::Create) =>
            {
                let creation = create_private_directory_at(&directory, &name)?;
                (
                    open_directory_at(&directory, &name, SymlinkPolicy::Reject)?,
                    creation,
                )
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Cannot open private directory {}", path.display()));
            }
        };
        directory = next;
        let metadata = directory.metadata()?;
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid && (final_component || metadata.uid() != 0) {
            bail!(
                "Private storage requires an owned directory: {}",
                path.display()
            );
        }
        if final_component || matches!(creation, DirectoryCreation::Created) {
            directory.set_permissions(fs::Permissions::from_mode(0o700))?;
            clear_private_acl(&directory)?;
        } else {
            check_ancestor(&directory)?;
        }
    }
    Ok(PrivateDir { file: directory })
}

fn check_private_directory(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("Private storage requires an owned directory");
    }
    file.set_permissions(fs::Permissions::from_mode(0o700))?;
    clear_private_acl(file)
}

fn check_user_regular(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!("Private storage requires an owned regular file with one link");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum SymlinkPolicy {
    Reject,
    TrustedAncestor,
}

#[derive(Debug, PartialEq, Eq)]
enum DirectoryCreation {
    Created,
    Existing,
}

fn open_directory_at(parent: &File, name: &CStr, policy: SymlinkPolicy) -> std::io::Result<File> {
    const DIRECTORY_OPEN_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    const PRIVATE_DIRECTORY_OPEN_FLAGS: libc::c_int = DIRECTORY_OPEN_FLAGS | libc::O_NOFOLLOW;
    let flags = match policy {
        SymlinkPolicy::Reject => PRIVATE_DIRECTORY_OPEN_FLAGS,
        SymlinkPolicy::TrustedAncestor => DIRECTORY_OPEN_FLAGS,
    };
    // SAFETY: parent owns a live descriptor and name is NUL-terminated.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor, including a valid fd 0.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_file_at(parent: &File, name: &CStr, flags: libc::c_int) -> std::io::Result<File> {
    // SAFETY: parent owns a live descriptor and name is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor, including fd 0.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn stat_child_at(parent: &File, name: &CStr) -> std::io::Result<libc::stat> {
    // SAFETY: all-zero is a valid initial value for the stat output structure.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: parent and name are valid; stat is writable.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &raw mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stat)
}

fn rename_child_at(parent: &File, source: &CStr, target: &CStr) -> std::io::Result<()> {
    // SAFETY: both names are NUL-terminated and parent owns its descriptor.
    if unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            target.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn unlink_child_at(parent: &File, name: &CStr) -> std::io::Result<()> {
    // SAFETY: name is NUL-terminated and parent owns its descriptor.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn root_owned_symlink_at(parent: &File, name: &CStr) -> bool {
    // SAFETY: all-zero is a valid initial value for the C stat output structure.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: parent and name are valid; stat is writable.
    (unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &raw mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    }) == 0
        && trusted_ancestor_alias(stat.st_mode, stat.st_uid)
}

fn trusted_ancestor_alias(mode: libc::mode_t, uid: libc::uid_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFLNK && uid == 0
}

fn directory_creation(result: libc::c_int, error: std::io::Error) -> Result<DirectoryCreation> {
    if result == 0 {
        Ok(DirectoryCreation::Created)
    } else if error.kind() == std::io::ErrorKind::AlreadyExists {
        Ok(DirectoryCreation::Existing)
    } else {
        Err(error.into())
    }
}

fn create_private_directory_at(parent: &File, name: &CStr) -> Result<DirectoryCreation> {
    // SAFETY: parent owns a live descriptor and name is NUL-terminated.
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    let creation = directory_creation(result, std::io::Error::last_os_error())?;
    if matches!(creation, DirectoryCreation::Created) {
        // Restore owner permissions removed by umask before opening the directory.
        // The new entry is owned by this user in the pinned, checked parent.
        // SAFETY: valid parent/name; no other user can replace our new entry.
        if unsafe { libc::fchmodat(parent.as_raw_fd(), name.as_ptr(), 0o700, 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(creation)
}

fn check_ancestor(file: &File) -> Result<()> {
    #[cfg(target_os = "macos")]
    mac_acl::check_ancestor(file)?;
    if writable_ancestor(file.metadata()?.mode()) {
        bail!("Private storage has a writable ancestor");
    }
    Ok(())
}

fn writable_ancestor(mode: u32) -> bool {
    mode & 0o022 != 0 && mode & 0o1000 == 0
}

/// Remove ACL grants and directory inheritance from private storage.
/// chmod alone does not remove macOS extended ACL grants.
pub fn clear_private_acl(file: &File) -> Result<()> {
    #[cfg(target_os = "linux")]
    for name in [c"system.posix_acl_access", c"system.posix_acl_default"] {
        // SAFETY: file owns a live descriptor and name is NUL-terminated.
        if unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) } != 0 {
            acl_removal_error(std::io::Error::last_os_error())?;
        }
    }
    #[cfg(target_os = "macos")]
    mac_acl::clear(file)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn acl_removal_error(error: std::io::Error) -> Result<()> {
    if matches!(error.raw_os_error(), Some(libc::ENODATA | libc::ENOTSUP)) {
        Ok(())
    } else {
        Err(error.into())
    }
}

#[cfg(target_os = "macos")]
mod mac_acl {
    use std::fs::File;
    use std::os::unix::io::AsRawFd as _;
    use std::ptr::NonNull;

    use anyhow::{Result, bail};

    // Darwin sys/acl.h; libc does not expose these functions.
    unsafe extern "C" {
        fn acl_init(count: libc::c_int) -> *mut libc::c_void;
        fn acl_get_fd(fd: libc::c_int) -> *mut libc::c_void;
        fn acl_set_fd(fd: libc::c_int, acl: *mut libc::c_void) -> libc::c_int;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            id: libc::c_int,
            entry: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut libc::c_void, tag: *mut libc::c_int) -> libc::c_int;
        fn acl_get_permset_mask_np(entry: *mut libc::c_void, mask: *mut u64) -> libc::c_int;
        fn acl_get_flagset_np(
            entry: *mut libc::c_void,
            flags: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_get_flag_np(flags: *mut libc::c_void, flag: libc::c_int) -> libc::c_int;
    }

    struct Acl(NonNull<libc::c_void>);

    impl Acl {
        fn from_raw(pointer: *mut libc::c_void) -> Result<Self> {
            NonNull::new(pointer)
                .map(Self)
                .ok_or_else(|| std::io::Error::last_os_error().into())
        }

        fn as_ptr(&self) -> *mut libc::c_void {
            self.0.as_ptr()
        }
    }

    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: this owns the allocation from acl_init or acl_get_fd.
            unsafe {
                acl_free(self.as_ptr());
            }
        }
    }

    pub(super) fn clear(file: &File) -> Result<()> {
        // SAFETY: acl_init allocates an empty extended ACL.
        let acl = Acl::from_raw(unsafe { acl_init(0) })?;
        // SAFETY: valid descriptor and allocated ACL.
        if unsafe { acl_set_fd(file.as_raw_fd(), acl.as_ptr()) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOTSUP) {
                return Err(error.into());
            }
        }
        Ok(())
    }

    pub(super) fn check_ancestor(file: &File) -> Result<()> {
        // ADD_FILE, DELETE, ADD_SUBDIRECTORY, DELETE_CHILD, WRITE_SECURITY, CHANGE_OWNER.
        const WRITE_CONTROL: u64 =
            (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 12) | (1 << 13);

        // SAFETY: file owns a valid descriptor.
        let pointer = unsafe { acl_get_fd(file.as_raw_fd()) };
        if pointer.is_null() {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTSUP)) {
                return Ok(());
            }
            return Err(error.into());
        }
        let acl = Acl::from_raw(pointer)?;
        let mut id = 0; // ACL_FIRST_ENTRY; ACL_NEXT_ENTRY is -1.
        loop {
            let mut entry = std::ptr::null_mut();
            // SAFETY: acl is allocated; entry is a writable output pointer.
            // Darwin acl_get_entry returns 0 for an entry and -1 with EINVAL
            // after the final entry (Apple acl_get_entry(3) RETURN VALUES).
            if unsafe { acl_get_entry(acl.as_ptr(), id, &raw mut entry) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINVAL) {
                    return Ok(());
                }
                return Err(error.into());
            }
            id = -1;
            let mut tag = 0;
            let mut mask = 0;
            let mut flags = std::ptr::null_mut();
            // SAFETY: entry belongs to the live ACL; outputs are writable.
            if unsafe { acl_get_tag_type(entry, &raw mut tag) } != 0
                || unsafe { acl_get_permset_mask_np(entry, &raw mut mask) } != 0
                || unsafe { acl_get_flagset_np(entry, &raw mut flags) } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            // ACL_ENTRY_ONLY_INHERIT does not authorize writes to this parent.
            // SAFETY: flags belongs to the live entry.
            let inherit_only = unsafe { acl_get_flag_np(flags, 1 << 8) };
            if inherit_only < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if tag == 1 && inherit_only == 0 && mask & WRITE_CONTROL != 0 {
                bail!("Private storage ancestor has an ACL granting write access");
            }
        }
    }
}

/// Write private JSON state atomically, tightening existing permissions.
pub fn atomic_write_json(path: &Path, json: &str) -> Result<()> {
    let directory = PrivateDir::create(path.parent().context("Private state has no parent")?)?;
    directory.write_atomic_private(
        path.file_name().context("Private state has no file name")?,
        json.as_bytes(),
        0o600,
    )
}

/// RAII file lock acquired via `flock(LOCK_EX)`. Releases on drop.
///
/// Use [`lock_sibling`] for an indefinite wait or [`lock_sibling_bounded`]
/// when a lifecycle operation must time out.
pub struct FileLock {
    _file: File,
}

/// Acquire an exclusive flock on a sibling `.lock` file next to `target`.
///
/// The lock file is created if necessary and lives across calls — its
/// purpose is purely to serialize access to `target`. Releases on drop.
/// Returns an error if the parent directory cannot be created or the
/// lock cannot be acquired.
#[mutants::skip] // low-payoff: flock helper; callers always target existing dirs, so the parent-dir guard is never exercised
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

/// Acquire a sibling lock with a bounded wait. The lock file stays outside
/// an instance directory, so destroying that directory cannot replace the
/// inode used by concurrent lifecycle operations.
pub fn lock_sibling_bounded(target: &Path, timeout: Duration) -> Result<FileLock> {
    let parent = target.parent().context("Lock target has no parent")?;
    fs::create_dir_all(parent)?;
    let stem = target.file_name().context("Lock target has no name")?;
    let path = parent.join(format!(".{}.operation.lock", stem.to_string_lossy()));
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Failed to open operation lock {}", path.display()))?;
    let start = Instant::now();
    loop {
        // SAFETY: file owns a valid descriptor throughout the flock call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(FileLock { _file: file });
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(error).with_context(|| format!("Failed to lock {}", path.display()));
        }
        if start.elapsed() >= timeout {
            bail!("Timed out waiting for operation lock {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
    let perms = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.nlink() != 1
            {
                bail!(
                    "Atomic write requires an owned regular file: {}",
                    path.display()
                );
            }
            let existing = metadata.permissions().mode() & 0o777;
            // Pick the more restrictive of (existing, requested).
            let combined = existing & mode;
            fs::Permissions::from_mode(combined)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::Permissions::from_mode(mode)
        }
        Err(error) => return Err(error.into()),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    clear_private_acl(temporary.as_file())?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().set_permissions(perms)?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to replace {}", path.display()))?;
    Ok(())
}

/// Write content to a file atomically, preserving SSH-appropriate
/// permissions (0o600 default).
pub fn atomic_write_ssh(path: &Path, content: &str) -> Result<()> {
    atomic_write_with_mode(path, content, 0o600)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {
    use super::*;

    #[test]
    fn sparse_copy_preserves_contents_and_holes() {
        let root = tempfile::tempdir().unwrap();
        let source = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(root.path().join("source"))
            .unwrap();
        let target_path = root.path().join("target");
        let target = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&target_path)
            .unwrap();
        let length = 64 * 1024 * 1024 + 3;
        source.set_len(length).unwrap();
        source.write_at(b"begin", 0).unwrap();
        source.write_at(b"end", length - 3).unwrap();
        target.set_len(128 * 1024 * 1024).unwrap();

        copy_sparse_file(&source, &target).unwrap();

        assert_eq!(target.metadata().unwrap().len(), length);
        let mut beginning = [0; 5];
        let mut middle = [1; 5];
        let mut end = [0; 3];
        target.read_at(&mut beginning, 0).unwrap();
        target.read_at(&mut middle, 32 * 1024 * 1024).unwrap();
        target.read_at(&mut end, length - 3).unwrap();
        assert_eq!(&beginning, b"begin");
        assert_eq!(&middle, &[0; 5]);
        assert_eq!(&end, b"end");
        assert!(target.metadata().unwrap().blocks() * 512 < 1024 * 1024);
    }

    #[test]
    fn directory_creation_preserves_existing_and_failed_results() {
        assert_eq!(
            directory_creation(0, std::io::Error::from_raw_os_error(libc::EACCES)).unwrap(),
            DirectoryCreation::Created
        );
        assert_eq!(
            directory_creation(-1, std::io::Error::from_raw_os_error(libc::EEXIST)).unwrap(),
            DirectoryCreation::Existing
        );
        assert!(directory_creation(-1, std::io::Error::from_raw_os_error(libc::EACCES)).is_err());
    }

    #[test]
    fn trusted_ancestor_alias_requires_root_owned_symlink() {
        assert!(trusted_ancestor_alias(libc::S_IFLNK | 0o777, 0));
        assert!(!trusted_ancestor_alias(libc::S_IFLNK | 0o777, 65534));
        assert!(!trusted_ancestor_alias(libc::S_IFDIR | 0o755, 0));
        assert!(!trusted_ancestor_alias(libc::S_IFREG | 0o644, 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn acl_error_policy_accepts_absence_and_unsupported_but_preserves_other_errors() {
        for errno in [libc::ENODATA, libc::ENOTSUP] {
            assert!(acl_removal_error(std::io::Error::from_raw_os_error(errno)).is_ok());
        }
        for errno in [libc::EACCES, libc::EIO] {
            let error = acl_removal_error(std::io::Error::from_raw_os_error(errno)).unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(errno)
            );
        }
    }

    #[test]
    fn existing_directory_repair_preserves_absence() {
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let path = root.path().join("missing/child");
        let error = private_existing_dir(&path).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(!root.path().join("missing").exists());
        private_dir(&path).unwrap();
        private_existing_dir(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn private_directory_creation_accepts_concurrent_callers() {
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let mut results = Vec::new();
                    for index in 0..32 {
                        barrier.wait();
                        results.push(private_dir(&root.path().join(format!("dir-{index}"))));
                    }
                    // All callers reach every barrier even when creation fails.
                    for result in results {
                        result.unwrap();
                    }
                });
            }
        });
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 32);
    }

    #[test]
    fn private_directory_enumeration_is_repeatable() {
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let directory = PrivateDir::create(&root.path().join("managed")).unwrap();
        directory
            .write_atomic_private(OsStr::new("one.json"), b"{}", 0o600)
            .unwrap();
        directory
            .write_atomic_private(OsStr::new("two.json"), b"{}", 0o600)
            .unwrap();
        let first = directory.entries().unwrap();
        let second = directory.entries().unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first, second);
    }

    #[test]
    fn private_handle_keeps_writes_in_validated_directory_after_name_replacement() {
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let original = root.path().join("managed");
        let directory = PrivateDir::create(&original).unwrap();
        let moved = root.path().join("moved");
        fs::rename(&original, &moved).unwrap();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &original).unwrap();
        directory
            .write_atomic_private(OsStr::new("state.json"), b"checked", 0o600)
            .unwrap();
        assert_eq!(
            fs::read_to_string(moved.join("state.json")).unwrap(),
            "checked"
        );
        assert!(!outside.join("state.json").exists());
    }

    #[test]
    fn open_for_update_repairs_read_only_private_file() {
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let directory = PrivateDir::create(root.path()).unwrap();
        let path = root.path().join("disk");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        let mut file = directory
            .open_regular_for_update(OsStr::new("disk"))
            .unwrap();
        file.write_all(b"more").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"more");
    }

    #[test]
    fn private_directory_descriptors_are_closed_with_stdin_initially_closed() {
        const CHILD: &str = "COOP_PRIVATE_DIRECTORY_FD_TEST";
        if std::env::var_os(CHILD).is_none() {
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "fs_util::tests::private_directory_descriptors_are_closed_with_stdin_initially_closed"])
                .env(CHILD, "1").status().unwrap().success());
            return;
        }
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        // The child runs only this test; fd 0 is deliberately available to openat.
        unsafe {
            libc::close(0);
        }
        assert!(fs::metadata(root.path().join("missing")).is_err());
        let directory = root.path().join("a/b");
        private_dir(&directory).unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(unsafe { libc::fcntl(0, libc::F_GETFD) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
    }

    #[test]
    fn writable_ancestor_requires_sticky_bit_for_each_write_class() {
        for mode in [0o700, 0o755, 0o1755, 0o1777, 0o1702, 0o1720] {
            assert!(!writable_ancestor(mode));
        }
        for mode in [0o702, 0o720, 0o722, 0o777] {
            assert!(writable_ancestor(mode));
        }
    }

    #[test]
    fn generic_config_writes_and_locks_preserve_parent_permissions() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let path = root.path().join("config.toml");
        atomic_write_with_mode(&path, "key = 1", 0o644).unwrap();
        let _lock = lock_sibling(&path).unwrap();
        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "key = 1");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_ancestor_acl_write_grants_are_rejected_and_deny_entries_are_allowed() {
        use std::process::Command;
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            Command::new("chmod")
                .args(["+a", "everyone allow add_file"])
                .arg(root.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(private_dir(&root.path().join("private")).is_err());
        assert!(
            Command::new("chmod")
                .arg("-N")
                .arg(root.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("chmod")
                .args(["+a", "everyone deny delete"])
                .arg(root.path())
                .status()
                .unwrap()
                .success()
        );
        private_dir(&root.path().join("private")).unwrap();
        let file = root.path().join("private/state.json");
        fs::write(&file, "canary").unwrap();
        assert!(
            Command::new("chmod")
                .args(["+a", "everyone allow read"])
                .arg(&file)
                .status()
                .unwrap()
                .success()
        );
        crate::private_storage::private_file(&file).unwrap();
        let listing = Command::new("ls").arg("-le").arg(&file).output().unwrap();
        assert!(listing.status.success());
        assert!(!String::from_utf8_lossy(&listing.stdout).contains("allow read"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "canary");
    }

    #[test]
    fn bounded_sibling_lock_times_out_and_can_be_reacquired() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("instance");
        let first = lock_sibling_bounded(&target, Duration::from_millis(100)).unwrap();
        let error = lock_sibling_bounded(&target, Duration::from_millis(100))
            .err()
            .unwrap();
        assert!(error.to_string().contains("Timed out"));
        drop(first);
        assert!(lock_sibling_bounded(&target, Duration::from_millis(100)).is_ok());
    }

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
        let dir = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
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
        assert_eq!(mode, 0o600);
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
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn atomic_write_with_mode_never_relaxes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        // Requesting a more permissive mode must not widen the file.
        atomic_write_with_mode(&path, "new", 0o644).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "existing 0o600 must not be relaxed to 0o644");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn atomic_write_with_mode_default_for_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new");
        atomic_write_with_mode(&path, "content", 0o640).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "new file must use the requested mode verbatim");
        assert_eq!(fs::read_to_string(&path).unwrap(), "content");
    }

    #[test]
    fn atomic_write_with_mode_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("dir").join("file");
        atomic_write_with_mode(&path, "content", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "content");
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
