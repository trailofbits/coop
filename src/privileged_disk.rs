//! Privileged Firecracker disk operations. The helper reopens and validates
//! managed paths *after* sudo, then uses pinned directories and files.
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, bail};

use crate::cmd::Cmd;

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
    fn program(self) -> &'static str {
        match self {
            Self::FsckFix | Self::FsckRead => "/usr/sbin/e2fsck",
            Self::Resize => "/usr/sbin/resize2fs",
            Self::Format => "/usr/sbin/mkfs.ext4",
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
    let lock = open_copy_scratch(&dst.parent, c".coop-disk-copy.lock", uid)?;
    // SAFETY: lock is a live regular file in the checked private directory.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let name = c".coop-disk-copy.tmp";
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
    gib.checked_mul(1024 * 1024 * 1024)
        .context("Disk size overflow")
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
        "format" if argument.is_none() => format_disk(&disk, uid),
        "mount" if argument.is_none() => mount_disk(&disk, uid),
        "unmount" if argument.is_none() => unmount_target(&disk, None),
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

fn proc_fd(file: &File) -> String {
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

fn mount_disk(disk: &DiskPath, uid: u32) -> Result<()> {
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
fn run_on_descriptors(tool: DiskTool, args: &[&str], descriptors: &[&File]) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    let fds: Vec<_> = descriptors.iter().map(|file| file.as_raw_fd()).collect();
    let program = tool.program();
    let mut command = Command::new(program);
    command.args(args);
    // SAFETY: pre_exec performs only async-signal-safe fcntl calls. The fd
    // vector is captured before fork and each fd is owned by a live File.
    unsafe {
        command.pre_exec(move || {
            for fd in &fds {
                if libc::fcntl(*fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let status = command
        .status()
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
    run_on_descriptors(tool, &args, &[&file])
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
    )
}

fn invoke(operation: &str, path: &Path, argument: Option<&OsStr>) -> Result<()> {
    let root = path
        .ancestors()
        .nth(3)
        .context("Disk path has no data root")?;
    let mut command = Cmd::new(running_executable_path())
        .arg("__disk-op")
        .arg(operation)
        .arg(root)
        .arg(path);
    if let Some(argument) = argument {
        command = command.arg(argument);
    }
    command.sudo().run()
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
    invoke("remove", path, None)
}
pub(crate) fn copy_disk(src: &Path, dst: &Path) -> Result<()> {
    invoke("copy", src, Some(dst.as_os_str()))
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
    invoke("swap", path, None)
}
pub(crate) fn mount(path: &Path) -> Result<()> {
    invoke("mount", path, None)
}
pub(crate) fn unmount(path: &Path) -> Result<()> {
    invoke("unmount", path, None)
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
