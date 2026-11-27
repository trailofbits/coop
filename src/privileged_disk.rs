//! Privileged Firecracker disk operations. The helper reopens and validates
//! managed paths *after* sudo, then uses pinned directories and files.
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus,
};
use rustix::fs::{AtFlags, StatxFlags, statx};

use crate::setup_tool::SetupTool;

const COPY_LOCK_NAME: &CStr = c".coop-disk-copy.lock";
const COPY_TMP_NAME: &CStr = c".coop-disk-copy.tmp";
#[cfg(target_arch = "x86_64")]
const DISK_TOOL_AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const DISK_TOOL_AUDIT_ARCH: u32 = 0xc000_00b7;

struct DiskPath {
    root: File,
    parent: File,
    name: CString,
    kind: DiskKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiskKind {
    Template,
    Instance,
}

#[derive(Clone, Copy, Debug)]
enum DiskTool {
    FsckFix,
    FsckRead,
    Resize,
    Format,
}

impl DiskTool {
    fn setup_role(self) -> SetupTool {
        match self {
            Self::FsckFix | Self::FsckRead => SetupTool::E2fsck,
            Self::Resize => SetupTool::Resize2fs,
            Self::Format => SetupTool::MkfsExt4,
        }
    }

    fn succeeded(self, status: ExitStatus) -> bool {
        if status.success() {
            return true;
        }
        match self {
            // e2fsck(8): 1 means errors corrected; other bits still require failure.
            Self::FsckFix => status.code() == Some(1),
            Self::FsckRead | Self::Resize | Self::Format => false,
        }
    }
}

const LOOP_CTL_GET_FREE: libc::Ioctl = libc::_IO(0x4c, 0x82);
const LOOP_SET_FD: libc::Ioctl = libc::_IO(0x4c, 0x00);
const LOOP_CLR_FD: libc::Ioctl = libc::_IO(0x4c, 0x01);
const LOOP_SET_STATUS64: libc::Ioctl = libc::_IO(0x4c, 0x04);

fn invoking_uid() -> Result<u32> {
    // A direct invocation is useful for rootless tests. Under sudo the real
    // caller is SUDO_UID, and the helper must never trust an argv uid.
    let uid = match std::env::var("SUDO_UID") {
        Ok(value) => value.parse().context("Invalid SUDO_UID")?,
        Err(_) => unsafe { libc::geteuid() }, // SAFETY: geteuid has no preconditions.
    };
    if uid == 0 && unsafe { libc::geteuid() } == 0 {
        bail!("Privileged disk helper requires an unprivileged invoking user");
    }
    Ok(uid)
}

fn c_name(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).context("Filesystem component contains NUL")
}

fn open_at(parent: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // SAFETY: parent owns its fd; name is NUL-terminated; flags request a new fd.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returns a new owned descriptor, including fd 0.
    Ok(unsafe { File::from_raw_fd(fd) })
}

impl DiskPath {
    fn open(root_path: &Path, path: &Path, uid: u32) -> Result<Self> {
        let name = path.file_name().context("Disk has no filename")?;
        let relative = path
            .strip_prefix(root_path)
            .context("Disk is outside configured storage root")?;
        let parts: Vec<_> = relative.components().collect();
        let [
            Component::Normal(kind),
            Component::Normal(entry),
            Component::Normal(_),
        ] = parts.as_slice()
        else {
            bail!("Disk is outside managed storage layout: {}", path.display());
        };
        let allowed = matches!(
            (kind.to_str(), name.to_str()),
            (
                Some("images"),
                Some("rootfs-template.ext4" | "rootfs-template.ext4.new")
            ) | (Some("instances"), Some("rootfs.ext4"))
        );
        if !allowed || !path.is_absolute() || !root_path.is_absolute() {
            bail!("Not a managed Firecracker disk: {}", path.display());
        }
        match kind.to_str() {
            Some("images") => {
                crate::config::ImageName::new(entry.to_str().context("Non-UTF8 image name")?)?;
            }
            Some("instances") => {
                crate::config::InstanceName::new(
                    entry.to_str().context("Non-UTF8 instance name")?,
                )?;
            }
            _ => unreachable!("allowed layout was checked above"),
        }
        let mut directory = File::open("/")?;
        for component in root_path.components() {
            let Component::Normal(part) = component else {
                if matches!(component, Component::ParentDir) {
                    bail!("Disk path contains '..': {}", path.display());
                }
                continue;
            };
            let name = c_name(part)?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
            directory = match open_at(&directory, &name, flags) {
                Ok(next) => next,
                Err(_error) if is_root_owned_symlink(&directory, &name) => {
                    open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)?
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Cannot open storage root {}", root_path.display())
                    });
                }
            };
            let metadata = directory.metadata()?;
            if metadata.uid() != 0 && metadata.uid() != uid {
                bail!("Foreign-owned ancestor of managed disk: {}", path.display());
            }
            if metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
                bail!(
                    "Writable non-sticky ancestor of managed disk: {}",
                    path.display()
                );
            }
        }
        check_private_directory(&directory, uid)?;
        let root = directory;
        let collection = open_at(
            &root,
            &c_name(kind)?,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )?;
        check_private_directory(&collection, uid)?;
        let directory = open_at(
            &collection,
            &c_name(entry)?,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )?;
        check_private_directory(&directory, uid)?;
        Ok(Self {
            root,
            parent: directory,
            name: c_name(name)?,
            kind: if *kind == "images" {
                DiskKind::Template
            } else {
                DiskKind::Instance
            },
        })
    }

    fn file(&self, write: bool, uid: u32) -> Result<File> {
        let access = if write { libc::O_RDWR } else { libc::O_RDONLY };
        let file = open_at(
            &self.parent,
            &self.name,
            access | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || !matches!(metadata.uid(), 0) && metadata.uid() != uid
        {
            bail!("Managed disk must be a singly linked owned regular file");
        }
        Ok(file)
    }

    fn remove(&self, uid: u32) -> Result<()> {
        ensure_unmounted(self)?;
        match self.file(false, uid) {
            Ok(_file) => {}
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        // The parent is private and pinned. Only the trusted host user/root can
        // change a child name; a guest cannot race it into another namespace.
        if unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

fn check_private_directory(directory: &File, uid: u32) -> Result<()> {
    let metadata = directory.metadata()?;
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        bail!("Managed disk directory is not owned and private");
    }
    Ok(())
}

fn is_root_owned_symlink(parent: &File, name: &CStr) -> bool {
    // SAFETY: all-zero is a valid initial stat output; parent and name are live.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &raw mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    }) == 0
        && stat.st_mode & libc::S_IFMT == libc::S_IFLNK
        && stat.st_uid == 0
}

fn copy(src: &DiskPath, dst: &DiskPath, uid: u32) -> Result<()> {
    let source = src.file(false, uid)?;
    ensure_unmounted(dst)?;
    // Refuse to replace a symlink, hardlink, foreign-owned file, or directory.
    if let Err(error) = dst.file(false, uid)
        && error
            .downcast_ref::<io::Error>()
            .is_none_or(|e| e.kind() != io::ErrorKind::NotFound)
    {
        return Err(error);
    }
    // A persistent lock serializes copies in this private directory. Reuse a
    // fixed staging name so a killed helper leaves at most one bounded file;
    // the next copy truncates it after taking the lock.
    let lock = open_copy_scratch(&dst.parent, COPY_LOCK_NAME, uid)?;
    // SAFETY: lock is a live regular file in the checked private directory.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let name = COPY_TMP_NAME;
    let target = open_copy_scratch(&dst.parent, name, uid)?;
    target.set_len(0)?;
    let result = (|| {
        let clone_result =
            unsafe { libc::ioctl(target.as_raw_fd(), libc::FICLONE as _, source.as_raw_fd()) };
        if clone_result != 0 {
            crate::fs_util::copy_sparse_file(&source, &target)?;
        }
        target.set_permissions(fs::Permissions::from_mode(0o600))?;
        target.sync_all()?;
        // SAFETY: descriptors and NUL-terminated names are valid. Both names
        // resolve only within the pinned private destination directory.
        if unsafe {
            libc::renameat(
                dst.parent.as_raw_fd(),
                name.as_ptr(),
                dst.parent.as_raw_fd(),
                dst.name.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        dst.parent.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: descriptor and name are valid; ignore cleanup failure.
        unsafe { libc::unlinkat(dst.parent.as_raw_fd(), name.as_ptr(), 0) };
    }
    result
}

fn open_copy_scratch(parent: &File, name: &CStr, uid: u32) -> Result<File> {
    let file = open_at(
        parent,
        name,
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || (metadata.uid() != 0 && metadata.uid() != uid)
    {
        bail!("Disk copy scratch entry must be a singly linked owned regular file");
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn size_bytes(size_gib: &str) -> Result<u64> {
    let gib: u64 = size_gib.parse().context("Invalid disk size")?;
    size_bytes_gib(gib).context("Disk size overflow")
}

fn size_bytes_gib(gib: u64) -> Option<u64> {
    gib.checked_mul(crate::config::GIB_BYTES)
}

#[cfg(kani)]
mod proofs {
    /// A requested GiB count either converts exactly to bytes or reports
    /// overflow before the privileged helper resizes a disk.
    #[kani::proof]
    fn privileged_disk_size_never_wraps() {
        let gib: u64 = kani::any();
        let converted = super::size_bytes_gib(gib);
        let gib_bytes = crate::config::GIB_BYTES;
        assert_eq!(converted.is_some(), gib <= u64::MAX / gib_bytes);
        if let Some(bytes) = converted {
            assert_eq!(bytes / gib_bytes, gib);
            assert_eq!(bytes % gib_bytes, 0);
        }
    }
}

pub(crate) fn run(operation: &str, root: &Path, path: &Path, argument: Option<&str>) -> Result<()> {
    let uid = invoking_uid()?;
    let disk = DiskPath::open(root, path, uid)?;
    match operation {
        "seal" if argument.is_none() => {
            let file = disk.file(false, uid)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            Ok(())
        }
        "remove" if argument.is_none() => disk.remove(uid),
        "copy" => copy(
            &disk,
            &DiskPath::open(
                root,
                Path::new(argument.context("Missing copy target")?),
                uid,
            )?,
            uid,
        ),
        "truncate" => {
            let file = disk.file(true, uid)?;
            let target = size_bytes(argument.context("Missing disk size")?)?;
            if target < file.metadata()?.len() {
                bail!("Shrinking a managed disk is not supported");
            }
            file.set_len(target)?;
            Ok(())
        }
        "fsck-fix" if argument.is_none() => {
            disk_tool(&disk, uid, DiskTool::FsckFix, &["-fy"], true)
        }
        "fsck-read" if argument.is_none() => {
            disk_tool(&disk, uid, DiskTool::FsckRead, &["-fn"], false)
        }
        "resize" if argument.is_none() => disk_tool(&disk, uid, DiskTool::Resize, &[], true),
        "sandbox-probe" if argument.is_none() => sandbox_probe(&disk, uid, DiskTool::Resize),
        "sandbox-probe-read" if argument.is_none() => sandbox_probe(&disk, uid, DiskTool::FsckRead),
        "format" if argument.is_none() => format_disk(&disk, uid),
        "mount" if argument.is_none() => mount_disk(&disk, uid),
        "unmount" if argument.is_none() => unmount_target(&disk, None),
        "recover-template-mount" if argument.is_none() => recover_template_mount(&disk),
        "ensure-unmounted" if argument.is_none() => ensure_unmounted(&disk),
        "mount-proc" if argument.is_none() => mount_child(&disk, "proc"),
        "mount-sys" if argument.is_none() => mount_child(&disk, "sys"),
        "mount-dev" if argument.is_none() => mount_child(&disk, "dev"),
        "mount-devpts" if argument.is_none() => mount_child(&disk, "dev/pts"),
        "mount-tmp" if argument.is_none() => mount_child(&disk, "tmp"),
        "unmount-proc" if argument.is_none() => unmount_target(&disk, Some("proc")),
        "unmount-sys" if argument.is_none() => unmount_target(&disk, Some("sys")),
        "unmount-dev" if argument.is_none() => unmount_target(&disk, Some("dev")),
        "unmount-devpts" if argument.is_none() => unmount_target(&disk, Some("dev/pts")),
        "unmount-tmp" if argument.is_none() => unmount_target(&disk, Some("tmp")),
        "resolv" if argument.is_none() => write_resolv(&disk),
        "swap" if argument.is_none() => {
            // Only the staging name may be promoted to the final template.
            if disk.name.as_bytes() != b"rootfs-template.ext4.new" {
                bail!("Only a template staging disk may be swapped");
            }
            let _staging = disk.file(false, uid)?;
            let final_name = c"rootfs-template.ext4";
            let final_disk = DiskPath {
                root: disk.root.try_clone()?,
                parent: disk.parent.try_clone()?,
                name: final_name.to_owned(),
                kind: disk.kind,
            };
            ensure_unmounted(&final_disk)?;
            if let Err(error) = final_disk.file(false, uid)
                && error
                    .downcast_ref::<io::Error>()
                    .is_none_or(|e| e.kind() != io::ErrorKind::NotFound)
            {
                return Err(error);
            }
            // SAFETY: both names are within the pinned private image directory.
            if unsafe {
                libc::renameat(
                    disk.parent.as_raw_fd(),
                    disk.name.as_ptr(),
                    disk.parent.as_raw_fd(),
                    final_name.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error().into());
            }
            disk.parent.sync_all()?;
            Ok(())
        }
        _ => bail!("Unsupported privileged disk operation"),
    }
}

fn mount_directory(disk: &DiskPath, create: bool) -> Result<File> {
    let base = if disk.kind == DiskKind::Instance {
        &disk.parent
    } else {
        &disk.root
    };
    let name = c"rootfs-mount";
    if create {
        // SAFETY: base is a pinned private directory and name is fixed.
        let result = unsafe { libc::mkdirat(base.as_raw_fd(), name.as_ptr(), 0o700) };
        if result != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
            return Err(io::Error::last_os_error().into());
        }
    }
    let directory = open_at(
        base,
        name,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )?;
    if create {
        // Before the loop mount, this is coop's host directory. After the
        // mount, metadata belongs to the guest-controlled filesystem root.
        let metadata = directory.metadata()?;
        if metadata.uid() != 0 && metadata.uid() != invoking_uid()? {
            bail!("Mount directory has foreign owner");
        }
        if metadata.mode() & 0o022 != 0 {
            bail!("Mount directory is group/world writable");
        }
    }
    Ok(directory)
}

fn mount_id(file: &File) -> Result<u64> {
    let stat = statx(file, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    if stat.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        bail!("Kernel did not report the mount ID");
    }
    Ok(stat.stx_mnt_id)
}

fn ensure_unmounted(disk: &DiskPath) -> Result<()> {
    let base = if disk.kind == DiskKind::Instance {
        &disk.parent
    } else {
        &disk.root
    };
    let mount = match open_at(
        base,
        c"rootfs-mount",
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    ) {
        Ok(mount) => mount,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if mount_id(&mount)? != mount_id(base)? {
        bail!("Rootfs mount is still active; unmount it before changing the disk");
    }
    Ok(())
}

fn proc_fd(file: &File) -> String {
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

/// The root helper is dedicated to one operation, so confine it before it
/// forks the filesystem parser. Its only writable regular file is the inode
/// already checked by `DiskPath::file`; an image's on-disk metadata is untrusted.
fn confine_filesystem_tool(disk: &File, program: &Path, tool: DiskTool) -> Result<()> {
    // These system trees contain the executable, loader, and shared libraries.
    // Their contents are readable but never executable through this rule.
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    let disk_access = if matches!(tool, DiskTool::FsckRead) {
        AccessFs::ReadFile.into()
    } else {
        AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate
    };
    let mut rules = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V3))?
        .create()?
        .add_rule(PathBeneath::new(disk.try_clone()?, disk_access))?
        .add_rule(PathBeneath::new(
            PathFd::new(program)?,
            AccessFs::ReadFile | AccessFs::Execute,
        ))?;
    for path in ["/usr/lib", "/usr/lib64", "/lib", "/lib64", "/nix/store"] {
        if Path::new(path).exists() {
            rules = rules.add_rule(PathBeneath::new(PathFd::new(path)?, read))?;
        }
    }
    // The kernel also executes a dynamic ELF's interpreter. Read PT_INTERP
    // from the trusted binary so only that loader is executable, including
    // non-FHS paths such as Nix's store.
    if let Some(interpreter) = elf_interpreter(program)? {
        let policy = crate::host_tool::TrustedToolPolicy::new(
            "/",
            ["/lib", "/lib64", "/usr/lib", "/usr/lib64", "/nix/store"].map(PathBuf::from),
            0,
        );
        let (canonical, metadata) = crate::host_tool::validate_trusted_path(&interpreter, &policy)
            .map_err(anyhow::Error::msg)
            .context("Filesystem tool ELF interpreter is not trusted")?;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            bail!("Filesystem tool ELF interpreter is not executable");
        }
        rules = rules.add_rule(PathBeneath::new(PathFd::new(canonical)?, AccessFs::Execute))?;
    }
    for path in [
        "/etc/ld.so.cache",
        "/etc/e2fsck.conf",
        "/sys/devices/system/cpu/possible",
    ] {
        if Path::new(path).exists() {
            rules = rules.add_rule(PathBeneath::new(PathFd::new(path)?, AccessFs::ReadFile))?;
        }
    }
    // e2fsprogs opens /proc/mounts through the calling task's /proc/self
    // symlink. A rule on one task's proc inode does not cover its child, so
    // allow file reads beneath procfs without directory enumeration. Landlock
    // also blocks ptrace-style access from this domain to outside processes.
    rules = rules.add_rule(PathBeneath::new(PathFd::new("/proc")?, AccessFs::ReadFile))?;
    rules = rules.add_rule(PathBeneath::new(
        PathFd::new("/dev/null")?,
        AccessFs::ReadFile | AccessFs::WriteFile,
    ))?;

    let status = rules.restrict_self()?;
    if !matches!(status.ruleset, RulesetStatus::FullyEnforced) {
        bail!("Disk tool filesystem sandbox is not fully enforced");
    }

    drop_disk_tool_capabilities(disk)?;
    Ok(())
}

fn elf_interpreter(program: &Path) -> Result<Option<PathBuf>> {
    let mut file = File::open(program)?;
    let length = file.metadata()?.len();
    let mut header = [0_u8; 64];
    file.read_exact(&mut header)?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        bail!("Sandboxed filesystem tool must be a little-endian ELF64 binary");
    }
    let ph_offset = u64::from_le_bytes(header[32..40].try_into()?);
    let ph_size = u64::from(u16::from_le_bytes(header[54..56].try_into()?));
    let ph_count = u64::from(u16::from_le_bytes(header[56..58].try_into()?));
    if ph_size < 56
        || ph_count > 256
        || ph_offset
            .checked_add(
                ph_size
                    .checked_mul(ph_count)
                    .context("ELF header overflow")?,
            )
            .is_none_or(|end| end > length)
    {
        bail!("Sandboxed filesystem tool has an invalid ELF program table");
    }
    for index in 0..ph_count {
        file.seek(SeekFrom::Start(ph_offset + index * ph_size))?;
        let mut entry = [0_u8; 56];
        file.read_exact(&mut entry)?;
        if u32::from_le_bytes(entry[..4].try_into()?) != 3 {
            continue;
        }
        let offset = u64::from_le_bytes(entry[8..16].try_into()?);
        let size = u64::from_le_bytes(entry[32..40].try_into()?);
        if !(2..=4096).contains(&size) || offset.checked_add(size).is_none_or(|end| end > length) {
            bail!("Sandboxed filesystem tool has an invalid ELF interpreter");
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; usize::try_from(size)?];
        file.read_exact(&mut bytes)?;
        let interpreter = CStr::from_bytes_with_nul(&bytes)
            .context("Sandboxed filesystem tool has an invalid ELF interpreter string")?;
        let path = PathBuf::from(OsStr::from_bytes(interpreter.to_bytes()));
        if !path.is_absolute() {
            bail!("Sandboxed filesystem tool has a relative ELF interpreter");
        }
        return Ok(Some(path));
    }
    Ok(None)
}

/// Landlock does not mediate chmod/chown, xattrs, timestamps, or signals. This
/// filter closes those root-owner escape paths, process creation, and sockets.
/// Install it in `pre_exec`: Rust uses a socket pair while spawning a child.
fn disk_tool_denied_syscalls() -> Vec<libc::c_long> {
    let common = [
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_clone,
        libc::SYS_clone3,
        libc::SYS_execveat,
        libc::SYS_memfd_create,
        libc::SYS_fchmod,
        libc::SYS_fchmodat,
        452, // fchmodat2 on the supported x86_64 and aarch64 Linux ABIs.
        libc::SYS_fchown,
        libc::SYS_fchownat,
        libc::SYS_setxattr,
        libc::SYS_lsetxattr,
        libc::SYS_fsetxattr,
        libc::SYS_removexattr,
        libc::SYS_lremovexattr,
        libc::SYS_fremovexattr,
        libc::SYS_utimensat,
        libc::SYS_kill,
        libc::SYS_tkill,
        libc::SYS_tgkill,
        libc::SYS_pidfd_send_signal,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_shmget,
        libc::SYS_shmat,
        libc::SYS_shmctl,
        libc::SYS_semget,
        libc::SYS_semop,
        libc::SYS_semctl,
        libc::SYS_msgget,
        libc::SYS_msgsnd,
        libc::SYS_msgrcv,
        libc::SYS_msgctl,
    ];
    #[cfg(target_arch = "x86_64")]
    let legacy_syscalls = [
        libc::SYS_fork,
        libc::SYS_vfork,
        libc::SYS_chmod,
        libc::SYS_chown,
        libc::SYS_lchown,
        libc::SYS_utime,
        libc::SYS_utimes,
        libc::SYS_futimesat,
    ];
    #[cfg(target_arch = "x86_64")]
    let syscalls = common.into_iter().chain(legacy_syscalls).collect();
    #[cfg(not(target_arch = "x86_64"))]
    let syscalls = common.to_vec();
    syscalls
}

fn disk_tool_syscall_filter() -> Result<Vec<libc::sock_filter>> {
    let load_field = |offset: u32| -> Result<libc::sock_filter> {
        Ok(libc::sock_filter {
            code: u16::try_from(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS)?,
            jt: 0,
            jf: 0,
            k: offset,
        })
    };
    let deny = libc::sock_filter {
        code: u16::try_from(libc::BPF_RET | libc::BPF_K)?,
        jt: 0,
        jf: 0,
        k: libc::SECCOMP_RET_ERRNO | u32::try_from(libc::EPERM)?,
    };
    let allow = libc::sock_filter {
        code: u16::try_from(libc::BPF_RET | libc::BPF_K)?,
        jt: 0,
        jf: 0,
        k: libc::SECCOMP_RET_ALLOW,
    };
    let mut filter = vec![
        load_field(u32::try_from(std::mem::offset_of!(
            libc::seccomp_data,
            arch
        ))?)?,
        libc::sock_filter {
            code: u16::try_from(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K)?,
            jt: 1,
            jf: 0,
            k: DISK_TOOL_AUDIT_ARCH,
        },
        deny,
        load_field(0)?,
    ];
    #[cfg(target_arch = "x86_64")]
    filter.extend([
        libc::sock_filter {
            code: u16::try_from(libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K)?,
            jt: 0,
            jf: 1,
            k: 0x4000_0000, // x32 syscall bit; those numbers differ from x86_64.
        },
        deny,
    ]);
    for number in disk_tool_denied_syscalls() {
        filter.push(libc::sock_filter {
            code: u16::try_from(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K)?,
            jt: 0,
            jf: 1,
            k: u32::try_from(number)?,
        });
        filter.push(deny);
    }
    filter.push(allow);
    Ok(filter)
}

fn install_disk_tool_syscall_filter(filter: &mut [libc::sock_filter]) -> io::Result<()> {
    let Ok(len) = u16::try_from(filter.len()) else {
        return Err(io::Error::from_raw_os_error(libc::E2BIG));
    };
    let program = libc::sock_fprog {
        len,
        filter: filter.as_mut_ptr(),
    };
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &raw const program,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn drop_disk_tool_capabilities(disk: &File) -> Result<()> {
    // Lock the root-UID exception before clearing the sets: execve must not
    // silently restore capabilities to a UID-0 e2fsprogs process.
    let securebits = libc::SECBIT_NOROOT | libc::SECBIT_NOROOT_LOCKED;
    if unsafe { libc::prctl(libc::PR_SET_SECUREBITS, securebits) } != 0 {
        return Err(io::Error::last_os_error()).context("Cannot lock disk tool securebits");
    }
    if unsafe { libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL) } != 0 {
        return Err(io::Error::last_os_error())
            .context("Cannot clear disk tool ambient capabilities");
    }
    for capability in 0..64 {
        if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, capability) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINVAL) {
                return Err(error).context("Cannot clear disk tool capability bounding set");
            }
        }
    }
    if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
        return Err(io::Error::last_os_error()).context("Cannot clear disk tool groups");
    }
    let owner = disk.metadata()?;
    if unsafe { libc::setresgid(owner.gid(), owner.gid(), owner.gid()) } != 0 {
        return Err(io::Error::last_os_error()).context("Cannot set disk tool group");
    }
    if unsafe { libc::setresuid(owner.uid(), owner.uid(), owner.uid()) } != 0 {
        return Err(io::Error::last_os_error()).context("Cannot set disk tool user");
    }
    let header = CapabilityHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let empty = CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    };
    let data = [empty; 2];
    if unsafe { libc::syscall(libc::SYS_capset, &raw const header, &raw const data) } != 0 {
        return Err(io::Error::last_os_error()).context("Cannot clear disk tool capabilities");
    }
    Ok(())
}

/// Exercise the same boundary as a real filesystem tool, using only harmless
/// opens and a fixed no-op executable. Called by the privileged disk probe.
fn sandbox_probe(disk: &DiskPath, uid: u32, tool: DiskTool) -> Result<()> {
    let read_only = matches!(tool, DiskTool::FsckRead);
    let file = disk.file(!read_only, uid)?;
    let executable = tool.setup_role().command()?.build();
    confine_filesystem_tool(&file, Path::new(executable.get_program()), tool)?;
    match File::open("/etc/passwd") {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        result => bail!("Disk tool could read an unrelated host file: {result:?}"),
    }
    match File::open("/proc/1/environ") {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        result => bail!("Disk tool could read another process's environment: {result:?}"),
    }
    match fs::OpenOptions::new().write(true).open("/etc/passwd") {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        result => bail!("Disk tool could open an unrelated host file for writing: {result:?}"),
    }
    match std::process::Command::new("/usr/bin/true").status() {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        result => bail!("Disk tool could launch an unrelated program: {result:?}"),
    }
    let mut syscall_filter = disk_tool_syscall_filter()?;
    install_disk_tool_syscall_filter(&mut syscall_filter)
        .context("Cannot block disk tool syscalls in probe")?;
    match std::os::unix::net::UnixStream::pair() {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        result => bail!("Disk tool could create a Unix socket pair: {result:?}"),
    }
    probe_process_restrictions()?;
    match std::net::TcpStream::connect("127.0.0.1:1") {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        result => bail!("Disk tool could create a TCP connection: {result:?}"),
    }
    match file.set_permissions(file.metadata()?.permissions()) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        result => bail!("Disk tool could change file permissions: {result:?}"),
    }
    if unsafe { libc::kill(1, 0) } != -1
        || io::Error::last_os_error().kind() != io::ErrorKind::PermissionDenied
    {
        bail!("Disk tool could signal another host process");
    }
    if read_only {
        match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(proc_fd(&file))
        {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            result => bail!("Read-only disk tool could write its image: {result:?}"),
        }
        File::open(proc_fd(&file))?;
    } else {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(proc_fd(&file))?;
    }
    let header = CapabilityHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let empty = CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    };
    let mut data = [empty; 2];
    if unsafe { libc::syscall(libc::SYS_capget, &raw const header, &raw mut data) } != 0 {
        return Err(io::Error::last_os_error()).context("Cannot inspect disk tool capabilities");
    }
    if data
        .iter()
        .any(|entry| entry.effective != 0 || entry.permitted != 0 || entry.inheritable != 0)
    {
        bail!("Disk tool retained capabilities");
    }
    Ok(())
}

fn probe_process_restrictions() -> Result<()> {
    let forked = unsafe { libc::fork() };
    if forked == 0 {
        unsafe { libc::_exit(0) };
    }
    if forked > 0 {
        unsafe { libc::waitpid(forked, std::ptr::null_mut(), 0) };
        bail!("Disk tool could create a child process");
    }
    if io::Error::last_os_error().kind() != io::ErrorKind::PermissionDenied {
        bail!("Disk tool fork failed for a reason other than the sandbox");
    }
    if unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"coop-disk-probe".as_ptr(),
            libc::MFD_CLOEXEC,
        )
    } != -1
        || io::Error::last_os_error().kind() != io::ErrorKind::PermissionDenied
    {
        bail!("Disk tool could create executable anonymous memory");
    }
    if unsafe {
        libc::syscall(
            libc::SYS_execveat,
            -1,
            c"".as_ptr(),
            std::ptr::null::<*const libc::c_char>(),
            std::ptr::null::<*const libc::c_char>(),
            libc::AT_EMPTY_PATH,
        )
    } != -1
        || io::Error::last_os_error().kind() != io::ErrorKind::PermissionDenied
    {
        bail!("Disk tool could execute an anonymous file descriptor");
    }
    Ok(())
}

fn mount_disk(disk: &DiskPath, uid: u32) -> Result<()> {
    ensure_unmounted(disk)?;
    let source = disk.file(true, uid)?;
    let target = mount_directory(disk, true)?;
    let control = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/loop-control")?;
    let mut last_busy = None;
    for _ in 0..16 {
        // SAFETY: control owns a live loop-control descriptor; ioctl has no pointer argument.
        let number = unsafe { libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE) };
        if number < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let loop_path = format!("/dev/loop{number}");
        let loop_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&loop_path)?;
        if !loop_file.metadata()?.file_type().is_block_device() {
            bail!("Loop device is not a block device: {loop_path}");
        }
        // SAFETY: loop device and backing file descriptors are live. The ioctl
        // attaches precisely this already-validated backing file descriptor.
        if unsafe { libc::ioctl(loop_file.as_raw_fd(), LOOP_SET_FD, source.as_raw_fd()) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock
                || error.raw_os_error() == Some(libc::EBUSY)
            {
                last_busy = Some(error);
                continue;
            }
            return Err(error.into());
        }
        let mut info: LoopInfo64 = unsafe { std::mem::zeroed() }; // SAFETY: all-zero is valid loop_info64.
        info.lo_flags = 4; // LO_FLAGS_AUTOCLEAR
        // SAFETY: loop_file is attached; info points to a valid loop_info64.
        let status =
            unsafe { libc::ioctl(loop_file.as_raw_fd(), LOOP_SET_STATUS64, &raw const info) };
        if status != 0 {
            let error = io::Error::last_os_error();
            return Err(clear_loop_after_error(&loop_file, error));
        }
        let target_path = CString::new(proc_fd(&target))?;
        let source_path = CString::new(loop_path)?;
        // SAFETY: pointers are valid NUL-terminated names; target resolves to
        // the pinned directory and source is the loop device configured above.
        if unsafe {
            libc::mount(
                source_path.as_ptr(),
                target_path.as_ptr(),
                c"ext4".as_ptr(),
                0,
                std::ptr::null(),
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            return Err(clear_loop_after_error(&loop_file, error));
        }
        return Ok(());
    }
    Err(last_busy
        .context("No free loop device after retries")?
        .into())
}

fn clear_loop_after_error(loop_file: &File, cause: io::Error) -> anyhow::Error {
    // SAFETY: this live loop descriptor was attached by the helper, and failed
    // setup/mount has not exposed it as a filesystem.
    if unsafe { libc::ioctl(loop_file.as_raw_fd(), LOOP_CLR_FD) } != 0 {
        anyhow::anyhow!(
            "Disk mount failed: {cause}; loop-device cleanup also failed: {}",
            io::Error::last_os_error()
        )
    } else {
        cause.into()
    }
}

#[repr(C)]
#[expect(
    clippy::struct_field_names,
    reason = "matches the Linux loop_info64 ABI field names"
)]
struct LoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; 64],
    lo_crypt_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

fn mounted_child(disk: &DiskPath, child: &str) -> Result<File> {
    let mut current = mount_directory(disk, false)?;
    for part in child.split('/') {
        current = open_at(
            &current,
            &CString::new(part)?,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .with_context(|| format!("Guest mount target {child} is not a real directory"))?;
    }
    Ok(current)
}

fn mount_child(disk: &DiskPath, child: &str) -> Result<()> {
    if disk.kind != DiskKind::Template {
        bail!("Chroot mounts require a template image");
    }
    let target = mounted_child(disk, child)?;
    let target_path = CString::new(proc_fd(&target))?;
    let (source, fstype, flags) = match child {
        "proc" => (c"proc".as_ptr(), c"proc".as_ptr(), 0),
        "sys" => (c"sysfs".as_ptr(), c"sysfs".as_ptr(), 0),
        "dev" => (c"/dev".as_ptr(), std::ptr::null(), libc::MS_BIND),
        "dev/pts" => (c"devpts".as_ptr(), c"devpts".as_ptr(), 0),
        "tmp" => (c"tmpfs".as_ptr(), c"tmpfs".as_ptr(), 0),
        _ => bail!("Unsupported chroot mount target"),
    };
    // SAFETY: all pointers refer to live NUL-terminated strings; target is a
    // pinned directory descriptor opened with O_NOFOLLOW at every component.
    if unsafe {
        libc::mount(
            source,
            target_path.as_ptr(),
            fstype,
            flags,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn unmount_target(disk: &DiskPath, child: Option<&str>) -> Result<()> {
    let (parent, name) = match child {
        None => {
            let base = if disk.kind == DiskKind::Instance {
                &disk.parent
            } else {
                &disk.root
            };
            (base.try_clone()?, c"rootfs-mount".to_owned())
        }
        Some("dev/pts") => (mounted_child(disk, "dev")?, c"pts".to_owned()),
        Some(name @ ("proc" | "sys" | "dev" | "tmp")) => {
            (mount_directory(disk, false)?, CString::new(name)?)
        }
        _ => bail!("Unsupported chroot unmount target"),
    };
    unmount_checked_name(&parent, &name, || {})
}

/// Clean up a template mount left behind by an interrupted setup. The shared
/// mount lock must be held by the caller. Only the fixed mountpoint and its
/// fixed chroot children are touched; a busy mount is never detached.
fn recover_template_mount(disk: &DiskPath) -> Result<()> {
    if disk.kind != DiskKind::Template {
        bail!("Only a template mount can be recovered");
    }
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let mount = match mount_directory(disk, false) {
            Ok(mount) => mount,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if mount_id(&mount)? == mount_id(&disk.root)? {
            return Ok(());
        }
        drop(mount);

        let mut busy = false;
        for child in ["tmp", "dev/pts", "dev", "sys", "proc"] {
            match unmount_if_mounted(disk, Some(child)) {
                Ok(()) => {}
                Err(error) if is_busy(&error) => busy = true,
                Err(error) => {
                    return Err(error).with_context(|| format!("Failed to recover {child} mount"));
                }
            }
        }
        match unmount_if_mounted(disk, None) {
            Ok(()) if !busy => return Ok(()),
            Ok(()) => {}
            Err(error) if is_busy(&error) => busy = true,
            Err(error) => return Err(error).context("Failed to recover rootfs mount"),
        }
        if !busy {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Timed out waiting for interrupted rootfs mount to become idle");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_busy(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.raw_os_error() == Some(libc::EBUSY))
}

fn unmount_if_mounted(disk: &DiskPath, child: Option<&str>) -> Result<()> {
    let (parent, name) = match child {
        None => (disk.root.try_clone()?, c"rootfs-mount".to_owned()),
        Some("dev/pts") => {
            let dev = match mounted_child(disk, "dev") {
                Ok(dev) => dev,
                Err(error) if is_missing_mount_target(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            (dev, c"pts".to_owned())
        }
        Some(name @ ("proc" | "sys" | "dev" | "tmp")) => {
            (mount_directory(disk, false)?, CString::new(name)?)
        }
        _ => bail!("Unsupported chroot unmount target"),
    };
    let target = match open_at(
        &parent,
        &name,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    ) {
        Ok(target) => target,
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if mount_id(&target)? == mount_id(&parent)? {
        return Ok(());
    }
    drop(target);
    unmount_checked_name(&parent, &name, || {})
}

fn is_missing_mount_target(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|e| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)))
}

fn unmount_checked_name(parent: &File, name: &CStr, before_unmount: impl FnOnce()) -> Result<()> {
    // Reject an existing guest symlink and ask the kernel not to follow a
    // replacement introduced before umount resolves the final component.
    // Keeping the target fd open makes a normal unmount report EBUSY.
    drop(open_at(
        parent,
        name,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )?);
    before_unmount();
    let target_path = CString::new(format!("{}/{}", proc_fd(parent), name.to_string_lossy()))?;
    // SAFETY: parent is pinned and the final component is fixed. The kernel's
    // no-follow flag rejects a symlink substituted after the check above.
    if unsafe { libc::umount2(target_path.as_ptr(), libc::UMOUNT_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn write_resolv(disk: &DiskPath) -> Result<()> {
    if disk.kind != DiskKind::Template {
        bail!("resolv.conf injection requires a template image");
    }
    let target = mount_directory(disk, false)?;
    let content = fs::read("/etc/resolv.conf")?;
    crate::setup::replace_guest_file_in(target, &["etc"], "resolv.conf", &content)
}

/// The tool sees a procfs name for the already verified inode. We clear
/// CLOEXEC only in its child, never in coop's long-lived process. If a tool
/// forks and closes the descriptor, its open fails rather than re-resolving
/// the original untrusted path.
fn run_on_descriptors(
    tool: DiskTool,
    args: &[&str],
    descriptors: &[&File],
    sandbox_disk: Option<&File>,
) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    let fds: Vec<_> = descriptors.iter().map(|file| file.as_raw_fd()).collect();
    let program = tool.setup_role().name();
    let mut command = tool.setup_role().command()?.build();
    command.args(args);
    command.stdin(std::process::Stdio::null());
    let mut syscall_filter = sandbox_disk
        .map(|_| disk_tool_syscall_filter())
        .transpose()?;
    if let Some(disk) = sandbox_disk {
        confine_filesystem_tool(disk, Path::new(command.get_program()), tool)?;
    }
    // SAFETY: pre_exec uses only fcntl and prctl on descriptors and a filter
    // prepared before fork. It does not allocate or acquire locks.
    unsafe {
        command.pre_exec(move || {
            for fd in &fds {
                if libc::fcntl(*fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if let Some(filter) = &mut syscall_filter {
                install_disk_tool_syscall_filter(filter)?;
            }
            Ok(())
        });
    }
    let status = crate::process_output::run_status_bounded(
        &mut command,
        crate::setup::SETUP_TOOL_DEADLINE,
        None,
    )
    .with_context(|| format!("Failed to run {program}"))?;
    if !tool.succeeded(status) {
        bail!("{program} failed with status {status}");
    }
    Ok(())
}

fn disk_tool(disk: &DiskPath, uid: u32, tool: DiskTool, flags: &[&str], write: bool) -> Result<()> {
    let file = disk.file(write, uid)?;
    let proc_path = format!("/proc/self/fd/{}", file.as_raw_fd());
    let mut args = flags.to_vec();
    args.push(&proc_path);
    run_on_descriptors(tool, &args, &[&file], Some(&file))
}

fn format_disk(disk: &DiskPath, uid: u32) -> Result<()> {
    if disk.name.as_bytes() != b"rootfs-template.ext4.new" {
        bail!("Only a template staging disk may be formatted");
    }
    let file = disk.file(true, uid)?;
    let unpack = open_at(
        &disk.root,
        c"squashfs-root",
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )?;
    let unpack_meta = unpack.metadata()?;
    if (unpack_meta.uid() != 0 && unpack_meta.uid() != uid) || unpack_meta.mode() & 0o022 != 0 {
        bail!("Unpacked rootfs directory has unsafe ownership or permissions");
    }
    let source_path = format!("/proc/self/fd/{}", unpack.as_raw_fd());
    let disk_path = format!("/proc/self/fd/{}", file.as_raw_fd());
    run_on_descriptors(
        DiskTool::Format,
        &["-d", &source_path, "-F", &disk_path],
        &[&unpack, &file],
        None,
    )
}

fn invoke(operation: &str, path: &Path, argument: Option<&OsStr>) -> Result<()> {
    let root = path
        .ancestors()
        .nth(3)
        .context("Disk path has no data root")?;
    let mut command = SetupTool::sudo_command(crate::setup::SETUP_TOOL_DEADLINE)?
        .arg(running_executable_path())
        .arg("__disk-op")
        .arg(operation)
        .arg(root)
        .arg(path);
    if let Some(argument) = argument {
        command = command.arg(argument);
    }
    command.run_bounded(crate::setup::SETUP_TOOL_DEADLINE)
}

/// Procfs resolves this to the running executable inode while the parent
/// waits for sudo. Replacing its original pathname cannot change root's code.
pub(crate) fn running_executable_path() -> PathBuf {
    PathBuf::from(format!("/proc/{}/exe", std::process::id()))
}

pub(crate) fn seal_existing(path: &Path) -> Result<()> {
    invoke("seal", path, None)
}
pub(crate) fn remove(path: &Path) -> Result<()> {
    let _mount_lock = lock_template_mount(path)?;
    invoke("remove", path, None)
}
pub(crate) fn ensure_disk_unmounted(path: &Path) -> Result<()> {
    invoke("ensure-unmounted", path, None)
}
pub(crate) fn copy_disk(src: &Path, dst: &Path) -> Result<()> {
    let _mount_lock = lock_template_mount(dst)?;
    invoke("copy", src, Some(dst.as_os_str()))
}

fn lock_template_mount(path: &Path) -> Result<Option<crate::fs_util::FileLock>> {
    let collection = path.ancestors().nth(2).context("Disk has no collection")?;
    if collection.file_name() != Some(OsStr::new("images")) {
        return Ok(None);
    }
    let root = collection.parent().context("Disk path has no data root")?;
    // Template images share one mountpoint. Wait for another image's mount
    // guard before the helper checks it, and hold the lock through mutation.
    // Mount/unmount calls already hold this lock through MountGuard.
    crate::fs_util::lock_sibling(&root.join("rootfs-mount")).map(Some)
}

pub(crate) fn cleanup_copy_scratch(directory: &Path) -> Result<()> {
    for name in [COPY_LOCK_NAME, COPY_TMP_NAME] {
        let path = directory.join(OsStr::from_bytes(name.to_bytes()));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
}
pub(crate) fn truncate_gib(path: &Path, size_gib: u32) -> Result<()> {
    invoke("truncate", path, Some(OsStr::new(&size_gib.to_string())))
}
pub(crate) fn fsck_fix(path: &Path) -> Result<()> {
    invoke("fsck-fix", path, None)
}
pub(crate) fn fsck_read(path: &Path) -> Result<()> {
    invoke("fsck-read", path, None)
}
pub(crate) fn resize(path: &Path) -> Result<()> {
    invoke("resize", path, None)
}
pub(crate) fn format(path: &Path) -> Result<()> {
    invoke("format", path, None)
}
pub(crate) fn swap(path: &Path) -> Result<()> {
    let _mount_lock = lock_template_mount(path)?;
    invoke("swap", path, None)
}
pub(crate) fn mount(path: &Path) -> Result<()> {
    invoke("mount", path, None)
}
pub(crate) fn unmount(path: &Path) -> Result<()> {
    invoke("unmount", path, None)
}
/// The caller already owns the shared template mount lock.
pub(crate) fn recover_template_mount_under_lock(path: &Path) -> Result<()> {
    invoke("recover-template-mount", path, None)
}
pub(crate) fn recover_stale_template_mount(path: &Path) -> Result<()> {
    let _mount_lock = lock_template_mount(path)?;
    recover_template_mount_under_lock(path)
}
pub(crate) fn mount_sub(path: &Path, name: &str) -> Result<()> {
    invoke(&format!("mount-{name}"), path, None)
}
pub(crate) fn unmount_sub(path: &Path, name: &str) -> Result<()> {
    invoke(&format!("unmount-{name}"), path, None)
}
pub(crate) fn write_resolv_conf(path: &Path) -> Result<()> {
    invoke("resolv", path, None)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::os::unix::fs::{MetadataExt as _, symlink};
    use std::os::unix::process::ExitStatusExt as _;

    use super::*;

    const DISK_TOOLS: [DiskTool; 4] = [
        DiskTool::FsckFix,
        DiskTool::FsckRead,
        DiskTool::Resize,
        DiskTool::Format,
    ];

    #[test]
    fn size_bytes_rejects_overflow() {
        let maximum = u64::MAX / crate::config::GIB_BYTES;
        assert_eq!(
            size_bytes(&maximum.to_string()).unwrap(),
            maximum * crate::config::GIB_BYTES
        );
        assert!(size_bytes(&(maximum + 1).to_string()).is_err());
    }

    #[test]
    fn elf_interpreter_reads_only_a_bounded_absolute_path() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = b"/lib/ld-linux-aarch64.so.1\0";
        let mut elf = vec![0_u8; 120 + path.len()];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[32..40].copy_from_slice(&64_u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56_u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1_u16.to_le_bytes());
        elf[64..68].copy_from_slice(&3_u32.to_le_bytes());
        elf[72..80].copy_from_slice(&120_u64.to_le_bytes());
        elf[96..104].copy_from_slice(&(path.len() as u64).to_le_bytes());
        elf[120..].copy_from_slice(path);
        fs::write(file.path(), &elf).unwrap();
        assert_eq!(
            elf_interpreter(file.path()).unwrap(),
            Some(PathBuf::from("/lib/ld-linux-aarch64.so.1"))
        );
        elf[120] = b'.';
        fs::write(file.path(), &elf).unwrap();
        assert!(elf_interpreter(file.path()).is_err());
        elf[120] = b'/';
        elf[96..104].copy_from_slice(&5000_u64.to_le_bytes());
        fs::write(file.path(), &elf).unwrap();
        assert!(elf_interpreter(file.path()).is_err());
    }

    #[test]
    fn disk_tools_accept_clean_status() {
        for tool in DISK_TOOLS {
            assert!(tool.succeeded(ExitStatus::from_raw(0)), "{tool:?}");
        }
    }

    #[test]
    fn only_fsck_fix_accepts_corrected_status() {
        let corrected = ExitStatus::from_raw(1 << 8);
        assert!(DiskTool::FsckFix.succeeded(corrected));
        for tool in [DiskTool::FsckRead, DiskTool::Resize, DiskTool::Format] {
            assert!(!tool.succeeded(corrected), "{tool:?}");
        }
    }

    #[test]
    fn disk_tools_reject_errors_combined_statuses_and_signals() {
        for tool in DISK_TOOLS {
            for code in 2..=255 {
                assert!(
                    !tool.succeeded(ExitStatus::from_raw(code << 8)),
                    "{tool:?}: {code}"
                );
            }
            for signal in [
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGABRT,
                libc::SIGKILL,
                libc::SIGTERM,
            ] {
                assert!(
                    !tool.succeeded(ExitStatus::from_raw(signal)),
                    "{tool:?}: {signal}"
                );
            }
        }
    }

    #[test]
    fn mount_id_accepts_o_path_descriptor() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("mount")).unwrap();
        let parent = File::open(root.path()).unwrap();
        let child = open_at(
            &parent,
            c"mount",
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .unwrap();
        assert_eq!(mount_id(&child).unwrap(), mount_id(&parent).unwrap());
    }

    #[test]
    fn copy_disk_waits_for_shared_template_mount_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let root = tempfile::tempdir().unwrap();
        let mount_path = root.path().join("rootfs-mount");
        let mounting = crate::fs_util::lock_sibling(&mount_path).unwrap();
        let other_image = root.path().join("images/other/rootfs-template.ext4.new");
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            // This source reaches invoke's path validation after the wrapper
            // acquires the destination lock, then fails before any sudo call.
            let result = copy_disk(Path::new("source"), &other_image);
            finished_tx
                .send(result.map_err(|error| error.to_string()))
                .unwrap();
        });
        started_rx.recv().unwrap();
        let while_mounted = finished_rx.recv_timeout(Duration::from_millis(100));
        drop(mounting);
        let after_unmount = finished_rx.recv_timeout(Duration::from_secs(5));
        worker.join().unwrap();
        assert_eq!(while_mounted, Err(mpsc::RecvTimeoutError::Timeout));
        assert_eq!(
            after_unmount,
            Ok(Err("Disk path has no data root".to_string()))
        );

        let instance = root.path().join("instances/test/rootfs.ext4");
        assert!(lock_template_mount(&instance).unwrap().is_none());
    }

    struct TemporaryMount(CString);

    impl Drop for TemporaryMount {
        fn drop(&mut self) {
            // SAFETY: the target is the same NUL-terminated path mounted by
            // this test. Detach also handles a failed assertion or busy mount.
            unsafe { libc::umount2(self.0.as_ptr(), libc::MNT_DETACH) };
        }
    }

    fn mount_tmpfs(path: &Path) -> TemporaryMount {
        let target = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: all pointers are live NUL-terminated strings.
        assert_eq!(
            unsafe {
                libc::mount(
                    c"tmpfs".as_ptr(),
                    target.as_ptr(),
                    c"tmpfs".as_ptr(),
                    0,
                    std::ptr::null(),
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
        TemporaryMount(target)
    }

    #[test]
    #[ignore = "requires passwordless sudo and mount privileges"]
    fn unmount_rejects_name_swapped_to_outside_mount() {
        const CHILD: &str = "COOP_TEST_UNMOUNT_RACE";
        if std::env::var_os(CHILD).is_none() {
            let status = Command::new("sudo")
                .args(["-n", "env", "COOP_TEST_UNMOUNT_RACE=1"])
                .arg(format!("/proc/{}/exe", std::process::id()))
                .args([
                    "--exact",
                    "privileged_disk::tests::unmount_rejects_name_swapped_to_outside_mount",
                    "--ignored",
                ])
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let parent_path = root.path().join("parent");
        let outside = root.path().join("outside");
        fs::create_dir(&parent_path).unwrap();
        fs::create_dir(&outside).unwrap();
        let parent = File::open(&parent_path).unwrap();
        let host_device = parent.metadata().unwrap().dev();
        let _outside_mount = mount_tmpfs(&outside);
        assert_ne!(fs::metadata(&outside).unwrap().dev(), host_device);

        let target = parent_path.join("target");
        fs::create_dir(&target).unwrap();
        let result = unmount_checked_name(&parent, c"target", || {
            fs::remove_dir(&target).unwrap();
            symlink(&outside, &target).unwrap();
        });
        assert!(target.is_symlink());
        assert!(result.is_err());
        assert_ne!(fs::metadata(&outside).unwrap().dev(), host_device);

        let good = parent_path.join("good");
        fs::create_dir(&good).unwrap();
        let _good_mount = mount_tmpfs(&good);
        unmount_checked_name(&parent, c"good", || {}).unwrap();
        assert_eq!(fs::metadata(&good).unwrap().dev(), host_device);
    }
}
