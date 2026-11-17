//! Per-launch Firecracker jail layout. The jailer owns the mount and user
//! namespace transition and drops credentials; coop stages only fixed resources
//! before exec.

use std::fs;
use std::io::{self, Read as _, Seek as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Instance;
use crate::firecracker_service::ActiveService;
use crate::fs_util::PrivateDir;
use crate::host_tool::{TrustedToolPolicy, validate_trusted_path};
use crate::setup_tool::SetupTool;
use crate::sha256_hash::Sha256Hash;

// Keep the host-visible AF_UNIX socket path below sockaddr_un's 108-byte limit.
const JAIL_ROOT: &str = "/var/lib/coop/j";
const DEADLINE: Duration = Duration::from_secs(30);

pub(crate) fn require_jail_tools() -> Result<()> {
    for tool in [
        SetupTool::Sudo,
        SetupTool::Mkdir,
        SetupTool::Install,
        SetupTool::Setfacl,
        SetupTool::Ln,
        SetupTool::Rm,
        SetupTool::Mount,
        SetupTool::Umount,
        SetupTool::Cat,
        SetupTool::Stat,
        SetupTool::Journalctl,
    ] {
        tool.resolve()
            .with_context(|| format!("Firecracker jail requires trusted {}", tool.name()))?;
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct PreparedJail {
    pub(crate) id: String,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) base: PathBuf,
    pub(crate) root: PathBuf,
    pub(crate) kernel: PathBuf,
    disk: fs::File,
}

impl PreparedJail {
    pub(crate) fn uid_for_instance(inst: &Instance) -> Result<u32> {
        registered_uid(inst)
    }

    pub(crate) fn new(inst: &Instance, unit: &str, kernel: &Path) -> Result<Self> {
        let id = crate::firecracker_service::jail_id(inst, unit)?;
        let uid = Self::uid_for_instance(inst)?;
        // SAFETY: geteuid only reads the current process credential.
        let owner = unsafe { libc::geteuid() };
        let base = Path::new(JAIL_ROOT).join(owner.to_string());
        let root = base.join("firecracker").join(&id).join("root");
        if !kernel.is_absolute() {
            bail!("Kernel path must be absolute for jail staging");
        }
        if !inst.rootfs_path().is_absolute() {
            bail!("Instance disk path must be absolute for jail staging");
        }
        if !fs::metadata(kernel)
            .context("Cannot inspect kernel for jail staging")?
            .is_file()
        {
            bail!("Kernel for jail staging is not a regular file");
        }
        let disk = PrivateDir::open_existing(&inst.dir)?
            .pin_existing_regular(std::ffi::OsStr::new("rootfs.ext4"))?
            .context("Instance disk is missing")?;
        Ok(Self {
            id,
            uid,
            gid: uid,
            base,
            root,
            kernel: kernel.to_path_buf(),
            disk,
        })
    }

    pub(crate) fn disk_descriptor_path(&self) -> String {
        format!("/proc/{}/fd/{}", std::process::id(), self.disk.as_raw_fd())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "ordered root jail staging and access checks"
    )]
    pub(crate) fn prepare(&self, inst: &Instance, config: &[u8]) -> Result<()> {
        let policy = TrustedToolPolicy::new("/", [], 0);
        validate_trusted_path(Path::new("/var/lib/coop"), &policy)
            .map_err(anyhow::Error::msg)
            .context("Jail installation parent is not root-owned and trusted")?;
        let generation = self.root.parent().context("Jail root has no parent")?;
        let firecracker_base = self.base.join("firecracker");
        // SAFETY: geteuid only reads the current process credential.
        let owner = unsafe { libc::geteuid() };
        for (dir, permission) in [
            (self.base.as_path(), "r-x"),
            (firecracker_base.as_path(), "r-x"),
        ] {
            SetupTool::Mkdir
                .elevated_with_deadline(DEADLINE)?
                .args(["-p", "-m", "0700", "--"])
                .arg(dir)
                .run_bounded(DEADLINE)
                .with_context(|| {
                    format!(
                        "Cannot create root-owned Firecracker jail at {}",
                        dir.display()
                    )
                })?;
            SetupTool::Setfacl
                .elevated_with_deadline(DEADLINE)?
                .args(["-m", &format!("u:{owner}:{permission}"), "--"])
                .arg(dir)
                .run_bounded(DEADLINE)
                .with_context(|| {
                    format!("Cannot grant host owner jail access at {}", dir.display())
                })?;
        }
        match fs::symlink_metadata(generation) {
            Ok(_) => bail!("Fresh Firecracker jail generation already exists"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot inspect fresh jail generation"),
        }
        for dir in [generation, self.root.as_path()] {
            SetupTool::Mkdir
                .elevated_with_deadline(DEADLINE)?
                .args(["-p", "-m", "0700", "--"])
                .arg(dir)
                .run_bounded(DEADLINE)
                .with_context(|| format!("Cannot create jail generation at {}", dir.display()))?;
            SetupTool::Setfacl
                .elevated_with_deadline(DEADLINE)?
                .args(["-m", &format!("u:{owner}:--x"), "--"])
                .arg(dir)
                .run_bounded(DEADLINE)
                .with_context(|| {
                    format!(
                        "Cannot grant host owner jail traversal at {}",
                        dir.display()
                    )
                })?;
        }
        for parent in [
            Path::new(JAIL_ROOT),
            self.base.as_path(),
            &firecracker_base,
            generation,
        ] {
            validate_trusted_path(parent, &policy)
                .map_err(anyhow::Error::msg)
                .with_context(|| {
                    format!(
                        "Firecracker jail parent {} is not trusted",
                        parent.display()
                    )
                })?;
        }
        let mut staged =
            tempfile::NamedTempFile::new().context("Cannot stage Firecracker config")?;
        staged
            .write_all(config)
            .context("Cannot write staged Firecracker config")?;
        SetupTool::Install
            .elevated_with_deadline(DEADLINE)?
            .args(["-m", "0444", "/dev/stdin"])
            .arg(self.root.join("config.json"))
            .run_bounded_with_file(staged.reopen()?, DEADLINE)
            .context("Cannot install jail-relative Firecracker config")?;
        let kernel = fs::File::open(&self.kernel).context("Cannot open guest kernel for jail")?;
        SetupTool::Install
            .elevated_with_deadline(DEADLINE)?
            .args(["-m", "0444", "/dev/stdin"])
            .arg(self.root.join("kernel"))
            .run_bounded_with_file(kernel, DEADLINE)
            .context("Cannot copy guest kernel into jail")?;
        // The jailer opens an existing destination without truncating or
        // chmodding it. Precreate an empty executable so the service umask
        // cannot strip its execute bit when the jailer copies Firecracker.
        SetupTool::Install
            .elevated_with_deadline(DEADLINE)?
            .args(["-m", "0700", "/dev/null"])
            .arg(self.root.join("firecracker"))
            .run_bounded(DEADLINE)
            .context("Cannot stage jailed Firecracker executable")?;
        for name in ["rootfs.ext4", "firecracker.log"] {
            SetupTool::Install
                .elevated_with_deadline(DEADLINE)?
                .args(["-m", "0600", "/dev/null"])
                .arg(self.root.join(name))
                .run_bounded(DEADLINE)
                .with_context(|| format!("Cannot create jail {name} bind target"))?;
        }
        // The disk remains owned by the host user. Grant only this instance's
        // VMM UID access to the existing inode; recreate the ACL on every start
        // because restore or reprovision may have replaced the disk file.
        SetupTool::Setfacl
            .elevated_with_deadline(DEADLINE)?
            .args(["-m", &format!("u:{}:rw", self.uid), "--"])
            .arg(self.disk_descriptor_path())
            .run_bounded(DEADLINE)
            .context("Cannot grant the jailed VMM disk access")?;
        // The host owner needs only search access to the private jail path for
        // its log and AF_VSOCK socket. Other users cannot traverse the per-user
        // jail base. The jailer later changes root ownership to the VMM UID.
        SetupTool::Setfacl
            .elevated_with_deadline(DEADLINE)?
            .args(["-m", &format!("u:{}:rw,u:{owner}:r", self.uid), "--"])
            .arg(self.root.join("firecracker.log"))
            .run_bounded(DEADLINE)
            .context("Cannot grant VMM and host owner log access")?;
        symlink(self.root.join("firecracker.log"), inst.log_path())
            .context("Cannot publish jailed Firecracker log path")?;
        Ok(())
    }

    /// Roll back a staging failure before systemd has been asked to exec.
    pub(crate) fn cleanup_unstarted(&self, inst: &Instance) -> Result<()> {
        for (link, expected) in [(inst.log_path(), self.root.join("firecracker.log"))] {
            if fs::symlink_metadata(&link).is_ok() && fs::read_link(&link).ok() == Some(expected) {
                fs::remove_file(&link)
                    .with_context(|| format!("Cannot remove staged link {}", link.display()))?;
            }
        }
        if self.root.parent().is_some_and(Path::exists) {
            SetupTool::Rm
                .elevated_with_deadline(DEADLINE)?
                .args(["-rf", "--"])
                .arg(self.root.parent().context("Jail root has no parent")?)
                .run_bounded(DEADLINE)
                .context("Cannot remove unstarted Firecracker jail")?;
        }
        SetupTool::Setfacl
            .elevated_with_deadline(DEADLINE)?
            .args(["-x", &format!("u:{}", self.uid), "--"])
            .arg(self.disk_descriptor_path())
            .run_bounded(DEADLINE)
            .context("Cannot revoke unstarted VMM disk access")?;
        Ok(())
    }
}

fn assigned_uid(uid: u32) -> bool {
    // SAFETY: NSS returns borrowed records; only their presence is inspected.
    !unsafe { libc::getpwuid(uid) }.is_null() || !unsafe { libc::getgrgid(uid) }.is_null()
}

/// Reserve a distinct UID/GID with one atomic root-owned symlink. A killed
/// launcher leaves either a complete reservation or none, even if its `ln`
/// child outlives the host process. Another instance cannot claim that UID.
fn registered_uid(inst: &Instance) -> Result<u32> {
    let policy = TrustedToolPolicy::new("/", [], 0);
    validate_trusted_path(Path::new("/var/lib/coop"), &policy)
        .map_err(anyhow::Error::msg)
        .context("VMM identity parent is not root-owned and trusted")?;
    let registry = Path::new(JAIL_ROOT).join("identities");
    SetupTool::Mkdir
        .elevated_with_deadline(DEADLINE)?
        .args(["-p", "-m", "0755", "--"])
        .arg(&registry)
        .run_bounded(DEADLINE)
        .context("Cannot create root-owned VMM identity registry")?;
    validate_trusted_path(&registry, &policy)
        .map_err(anyhow::Error::msg)
        .context("VMM identity registry is not trusted")?;
    let digest = instance_identity_digest(inst)?;
    for attempt in 0..1024 {
        let uid = candidate_uid(&digest, attempt)?;
        let link = registry.join(uid.to_string());
        match fs::read_link(&link) {
            Ok(owner) if owner == Path::new(&digest) => {
                if assigned_uid(uid) {
                    bail!("VMM UID/GID {uid} now belongs to a host account");
                }
                return Ok(uid);
            }
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Invalid VMM UID reservation"),
        }
        if assigned_uid(uid) {
            continue;
        }
        let published = SetupTool::Ln
            .elevated_with_deadline(DEADLINE)?
            .args(["-s", "--"])
            .arg(&digest)
            .arg(&link)
            .run_bounded(DEADLINE);
        match published {
            Ok(()) => return Ok(uid),
            Err(error) => match fs::read_link(&link) {
                Ok(owner) if owner == Path::new(&digest) => return Ok(uid),
                Ok(_) => {}
                Err(_) => return Err(error).context("Cannot reserve VMM UID"),
            },
        }
    }
    bail!("No unassigned VMM UID could be reserved")
}

fn candidate_uid(digest: &str, attempt: u32) -> Result<u32> {
    let proposal = Sha256Hash::of(format!("{digest}:{attempt}")).to_string();
    let raw = u64::from_str_radix(&proposal[..16], 16)?;
    Ok(200_000 + u32::try_from(raw % 2_000_000_000)?)
}

fn existing_uid(inst: &Instance) -> Result<Option<u32>> {
    let registry = Path::new(JAIL_ROOT).join("identities");
    if !registry.exists() {
        return Ok(None);
    }
    let digest = instance_identity_digest(inst)?;
    for attempt in 0..1024 {
        let uid = candidate_uid(&digest, attempt)?;
        match fs::read_link(registry.join(uid.to_string())) {
            Ok(owner) if owner == Path::new(&digest) => return Ok(Some(uid)),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot inspect VMM UID reservation"),
        }
    }
    Ok(None)
}

fn instance_identity_digest(inst: &Instance) -> Result<String> {
    // SAFETY: geteuid only reads the current process credential.
    let owner = unsafe { libc::geteuid() };
    let mut bytes = owner.to_be_bytes().to_vec();
    bytes.extend(std::path::absolute(&inst.dir)?.as_os_str().as_bytes());
    Ok(Sha256Hash::of(bytes).to_string())
}

pub(crate) fn release_identity(inst: &Instance) -> Result<()> {
    let registry = Path::new(JAIL_ROOT).join("identities");
    if !registry.exists() {
        return Ok(());
    }
    let digest = instance_identity_digest(inst)?;
    for attempt in 0..1024 {
        let uid = candidate_uid(&digest, attempt)?;
        let link = registry.join(uid.to_string());
        match fs::read_link(&link) {
            Ok(owner) if owner == Path::new(&digest) => {
                SetupTool::Rm
                    .elevated_with_deadline(DEADLINE)?
                    .args(["-f", "--"])
                    .arg(&link)
                    .run_bounded(DEADLINE)
                    .context("Cannot release destroyed VMM identity")?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Invalid VMM UID reservation"),
        }
    }
    Ok(())
}

/// Resolve only a coop-generated log link for this instance.
pub(crate) fn checked_log_target(inst: &Instance, target: &Path) -> Result<PathBuf> {
    // SAFETY: geteuid only reads the current process credential.
    let owner = unsafe { libc::geteuid() };
    let base = Path::new(JAIL_ROOT)
        .join(owner.to_string())
        .join("firecracker");
    let relative = target
        .strip_prefix(&base)
        .context("Jailed log symlink points outside its owner namespace")?;
    let parts: Vec<_> = relative.components().collect();
    if parts.len() != 3
        || parts[1].as_os_str() != "root"
        || parts[2].as_os_str() != "firecracker.log"
    {
        bail!("Jailed log symlink has an invalid layout");
    }
    let id = parts[0]
        .as_os_str()
        .to_str()
        .context("Jailed log generation is not UTF-8")?;
    let digest = Sha256Hash::of(std::path::absolute(&inst.dir)?.as_os_str().as_bytes()).to_string();
    let generation = id
        .strip_prefix(&digest[..16])
        .and_then(|value| value.strip_prefix('-'))
        .context("Jailed log belongs to another instance")?;
    if generation.len() != 32
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("Jailed log has an invalid generation");
    }
    Ok(target.to_path_buf())
}

pub(crate) fn has_jailed_log_link(inst: &Instance) -> Result<bool> {
    let link = inst.log_path();
    match fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(&link)?;
            checked_log_target(inst, &target)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("Cannot inspect jailed log link"),
    }
}

pub(crate) fn open_log(inst: &Instance) -> Result<fs::File> {
    let path = inst.log_path();
    let target = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(&path).context("Cannot resolve jailed Firecracker log")?;
            checked_log_target(inst, &target)?
        }
        Ok(metadata) if metadata.file_type().is_file() => path,
        Ok(_) => bail!("Firecracker log path is not a regular file or managed jail link"),
        Err(error) => return Err(error).context("Cannot inspect Firecracker log path"),
    };
    open_regular_log(&target).context("Cannot open pinned Firecracker log")
}

/// Read a stopped instance's saved log, or its last service generation's
/// journal if cleanup has not yet replaced the managed link.
pub(crate) fn snapshot_stopped_log(inst: &Instance) -> Result<()> {
    const LIMIT: u64 = 16 * 1024 * 1024;
    let link = inst.log_path();
    let metadata = fs::symlink_metadata(&link)
        .with_context(|| format!("No saved Firecracker log for instance '{}'", inst.name))?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&link).context("Cannot read stopped Firecracker log link")?;
        let target = checked_log_target(inst, &target)?;
        let id = target
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .context("Stopped Firecracker log has no service generation")?;
        let generation = id
            .rsplit_once('-')
            .map(|(_, generation)| generation)
            .context("Stopped Firecracker log has no generation suffix")?;
        let unit = crate::firecracker_service::unit_for_generation(inst, generation)?;
        let journal = saved_journal(inst, generation, usize::try_from(LIMIT)?).with_context(|| {
            format!("Cannot read the stopped VM journal; try `sudo journalctl -u {unit}` on the host")
        })?;
        io::stdout().lock().write_all(&journal)?;
        return Ok(());
    }
    let mut file = open_regular_log(&link).context("Cannot open saved Firecracker log")?;
    let start = file.metadata()?.len().saturating_sub(LIMIT);
    file.seek(io::SeekFrom::Start(start))?;
    io::copy(&mut file.take(LIMIT), &mut io::stdout().lock())
        .context("Cannot print saved Firecracker log")?;
    Ok(())
}

fn open_regular_log(path: &Path) -> io::Result<fs::File> {
    // A dead VMM may leave a FIFO in its writable jail root. Open without
    // waiting for a writer, then reject anything that is not a regular file.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Firecracker log changed away from a regular file",
        ));
    }
    Ok(file)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn publish_vsock_mount(inst: &Instance, service: &ActiveService) -> Result<()> {
    let id = crate::firecracker_service::jail_id(inst, service.unit())?;
    // SAFETY: geteuid only reads the current process credential.
    let owner = unsafe { libc::geteuid() };
    let source = Path::new(JAIL_ROOT)
        .join(owner.to_string())
        .join("firecracker")
        .join(id)
        .join("root/vsock.sock");
    let started = std::time::Instant::now();
    loop {
        match fs::symlink_metadata(&source) {
            Ok(metadata) if metadata.file_type().is_socket() => break,
            Ok(_) => bail!("Jailed Firecracker vsock path is not a socket"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot inspect jailed vsock socket"),
        }
        if started.elapsed() >= Duration::from_secs(5) {
            bail!("Timed out waiting for jailed Firecracker vsock socket");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let c_source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
    // SAFETY: O_PATH pins the socket inode without connecting; O_NOFOLLOW
    // rejects a VMM-authored symlink at the final component.
    let fd = unsafe {
        libc::open(
            c_source.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot pin jailed vsock socket");
    }
    // SAFETY: open returned a new owned descriptor.
    let pinned = unsafe { fs::File::from_raw_fd(fd) };
    if !pinned.metadata()?.file_type().is_socket() {
        bail!("Jailed vsock socket changed identity before publication");
    }
    let destination = inst.vsock_path();
    let target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&destination)
        .context("Cannot create host vsock mount target")?;
    let descriptor_path = format!("/proc/{}/fd/{}", std::process::id(), pinned.as_raw_fd());
    let target_descriptor_path = format!("/proc/{}/fd/{}", std::process::id(), target.as_raw_fd());
    let mounted = SetupTool::Mount
        .elevated_with_deadline(DEADLINE)?
        .args(["--bind", &descriptor_path])
        .arg(&target_descriptor_path)
        .run_bounded(DEADLINE);
    if let Err(error) = mounted {
        if !is_mountpoint(&destination)? {
            fs::remove_file(&destination).context("Cannot remove failed vsock mount target")?;
        }
        return Err(error).context("Cannot bind the pinned jailed vsock socket to the instance");
    }
    let source_meta = pinned.metadata()?;
    let published_meta = fs::metadata(&destination)?;
    if (source_meta.dev(), source_meta.ino()) != (published_meta.dev(), published_meta.ino()) {
        bail!("Host vsock mount does not pin the jailed socket inode");
    }
    Ok(())
}

pub(crate) fn unmount_vsock(inst: &Instance) -> Result<()> {
    let path = inst.vsock_path();
    if !is_mountpoint(&path)? {
        return Ok(());
    }
    SetupTool::Umount
        .elevated_with_deadline(DEADLINE)?
        .arg("--")
        .arg(&path)
        .run_bounded(DEADLINE)
        .context("Cannot unmount stopped Firecracker vsock endpoint")?;
    if is_mountpoint(&path)? {
        bail!("Stopped Firecracker vsock endpoint remains mounted");
    }
    Ok(())
}

fn is_mountpoint(path: &Path) -> Result<bool> {
    let name = path
        .to_str()
        .context("Vsock mount path is not UTF-8")?
        .replace('\\', "\\134")
        .replace(' ', "\\040")
        .replace('\t', "\\011")
        .replace('\n', "\\012");
    let mountinfo =
        fs::read_to_string("/proc/self/mountinfo").context("Cannot inspect host mount table")?;
    Ok(mountinfo
        .lines()
        .any(|line| line.split_whitespace().nth(4) == Some(name.as_str())))
}

/// Confirm the process systemd supervises completed the jailer transition.
/// A successful systemd exec alone only proves the root jailer was started.
pub(crate) fn verify_vmm(inst: &Instance, service: &ActiveService) -> Result<()> {
    let uid = PreparedJail::uid_for_instance(inst)?;
    let pid = service.pid();
    let status = SetupTool::Cat
        .elevated_with_deadline(Duration::from_secs(5))?
        .arg(format!("/proc/{pid}/status"))
        .capture()
        .context("Cannot inspect jailed VMM credentials")?;
    verify_vmm_status(&status, uid)?;
    let id = crate::firecracker_service::jail_id(inst, service.unit())?;
    // SAFETY: geteuid only reads the current process credential.
    let owner = unsafe { libc::geteuid() };
    let expected_root = Path::new(JAIL_ROOT)
        .join(owner.to_string())
        .join("firecracker")
        .join(id)
        .join("root");
    let inode = |path: &Path| -> Result<String> {
        SetupTool::Stat
            .elevated_with_deadline(Duration::from_secs(5))?
            .args(["-Lc", "%d:%i", "--"])
            .arg(path)
            .capture()
            .context("Cannot inspect jailed VMM root inode")
    };
    let actual_root = inode(Path::new(&format!("/proc/{pid}/root")))?;
    let expected_root = inode(&expected_root)?;
    if actual_root.trim() != expected_root.trim() {
        bail!("Firecracker VMM root is outside its expected jail generation");
    }
    Ok(())
}

fn verify_vmm_status(status: &str, uid: u32) -> Result<()> {
    for key in ["Uid", "Gid"] {
        let values = status
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}:")))
            .context("Jailed VMM status lacks UID/GID")?;
        let fields: Vec<_> = values.split_whitespace().collect();
        let expected = uid.to_string();
        if fields.len() != 4 || fields.iter().any(|value| *value != expected) {
            bail!("Firecracker VMM {key} did not drop to its instance identity");
        }
    }
    for (key, expected) in [
        ("Groups", ""),
        ("CapEff", "0000000000000000"),
        ("NoNewPrivs", "1"),
        ("Seccomp", "2"),
    ] {
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}:")))
            .context("Jailed VMM status lacks a security field")?
            .trim();
        if value != expected {
            bail!("Firecracker VMM {key} does not match its jail policy");
        }
    }
    let bounding = status
        .lines()
        .find_map(|line| line.strip_prefix("CapBnd:"))
        .context("Jailed VMM status lacks a capability bounding set")?
        .trim();
    let bounding = u64::from_str_radix(bounding, 16)
        .context("Jailed VMM capability bounding set is not hexadecimal")?;
    // The root jailer needs other capabilities to stage mounts. Systemd
    // removes NET_ADMIN (capability 12) from the whole service before exec.
    if bounding & (1 << 12) != 0 {
        bail!("Firecracker VMM retains CAP_NET_ADMIN in its bounding set");
    }
    Ok(())
}

/// The jailer chmods the jailed root to 0700 after `pivot_root`, resetting the
/// ACL mask staged before exec. Restore only the host owner's search right.
pub(crate) fn grant_owner_access(inst: &Instance, service: &ActiveService) -> Result<()> {
    let id = crate::firecracker_service::jail_id(inst, service.unit())?;
    // SAFETY: geteuid only reads the current process credential.
    let owner = unsafe { libc::geteuid() };
    let root = Path::new(JAIL_ROOT)
        .join(owner.to_string())
        .join("firecracker")
        .join(id)
        .join("root");
    grant_owner_root_access(&root, owner)
}

fn grant_owner_root_access(root: &Path, owner: u32) -> Result<()> {
    SetupTool::Setfacl
        .elevated_with_deadline(DEADLINE)?
        .args(["-m", &format!("u:{owner}:--x"), "--"])
        .arg(root)
        .run_bounded(DEADLINE)
        .context("Cannot restore host owner access to Firecracker jail")
}

/// Reap only this instance's dead jail generations. This is also called on
/// restart after host reboot, when transient units have been unloaded.
pub(crate) fn cleanup_stopped(inst: &Instance) -> Result<()> {
    match crate::firecracker_service::probe(inst)? {
        crate::firecracker_service::ServiceProbe::None => {}
        crate::firecracker_service::ServiceProbe::Active(_) => {
            bail!("Cannot remove a running Firecracker jail");
        }
        crate::firecracker_service::ServiceProbe::Uncertain(reason) => bail!("{reason}"),
    }
    // SAFETY: geteuid only reads the current process credential.
    let owner = unsafe { libc::geteuid() };
    let base = Path::new(JAIL_ROOT).join(owner.to_string());
    let parent = base.join("firecracker");
    let policy = TrustedToolPolicy::new("/", [], 0);
    if !base.exists() {
        return revoke_disk_acl(inst);
    }
    validate_trusted_path(Path::new(JAIL_ROOT), &policy)
        .map_err(anyhow::Error::msg)
        .context("Firecracker jail installation parent is not trusted")?;
    // A launcher can die before publishing its host ACLs. Restore traversal
    // from the trusted root-owned parent before enumerating stale generations.
    SetupTool::Setfacl
        .elevated_with_deadline(DEADLINE)?
        .args(["-m", &format!("u:{owner}:r-x"), "--"])
        .arg(&base)
        .run_bounded(DEADLINE)
        .context("Cannot inspect stopped Firecracker jail base")?;
    if !parent.exists() {
        return revoke_disk_acl(inst);
    }
    SetupTool::Setfacl
        .elevated_with_deadline(DEADLINE)?
        .args(["-m", &format!("u:{owner}:r-x"), "--"])
        .arg(&parent)
        .run_bounded(DEADLINE)
        .context("Cannot inspect stopped Firecracker jail generations")?;
    let entries = match fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return revoke_disk_acl(inst);
        }
        Err(error) => return Err(error).context("Cannot enumerate stopped Firecracker jails"),
    };
    validate_trusted_path(&parent, &policy)
        .map_err(anyhow::Error::msg)
        .context("Firecracker jail parent changed ownership or permissions")?;
    let digest = Sha256Hash::of(std::path::absolute(&inst.dir)?.as_os_str().as_bytes()).to_string();
    let prefix = format!("{}-", &digest[..16]);
    for entry in entries {
        let entry = entry.context("Cannot read Firecracker jail entry")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(generation) = name.strip_prefix(&prefix) else {
            continue;
        };
        if generation.len() != 32
            || !generation
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("Firecracker jail has an invalid generation name");
        }
        if !entry.file_type()?.is_dir() {
            bail!("Firecracker jail generation is not a directory");
        }
        let generation_path = entry.path();
        let jail_root = generation_path.join("root");
        if jail_root.exists() {
            grant_owner_root_access(&jail_root, owner)?;
        }
        preserve_log(inst, &jail_root, generation)?;
        SetupTool::Rm
            .elevated_with_deadline(DEADLINE)?
            .args(["-rf", "--"])
            .arg(&generation_path)
            .run_bounded(DEADLINE)
            .with_context(|| format!("Cannot remove stopped jail generation {name}"))?;
    }
    revoke_disk_acl(inst)
}

fn revoke_disk_acl(inst: &Instance) -> Result<()> {
    if let Some(uid) = existing_uid(inst)?
        && let Some(disk) = PrivateDir::open_existing(&inst.dir)?
            .pin_existing_regular(std::ffi::OsStr::new("rootfs.ext4"))?
    {
        let descriptor_path = format!("/proc/{}/fd/{}", std::process::id(), disk.as_raw_fd());
        SetupTool::Setfacl
            .elevated_with_deadline(DEADLINE)?
            .args(["-x", &format!("u:{uid}"), "--"])
            .arg(descriptor_path)
            .run_bounded(DEADLINE)
            .context("Cannot revoke stopped VMM disk access")?;
    }
    Ok(())
}

fn preserve_log(inst: &Instance, jail_root: &Path, generation: &str) -> Result<()> {
    const MAX_SAVED_LOG: u64 = 16 * 1024 * 1024;
    let link = inst.log_path();
    let metadata = match fs::symlink_metadata(&link) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("Cannot inspect jailed log link"),
    };
    if !metadata.file_type().is_symlink()
        || fs::read_link(&link)? != jail_root.join("firecracker.log")
    {
        return Ok(());
    }
    let mut staged = tempfile::NamedTempFile::new_in(&inst.dir)
        .context("Cannot stage stopped Firecracker log")?;
    // The jailed path is writable by the VMM and may have been replaced with
    // a FIFO, symlink, or device. Its stderr is sent to the system journal,
    // which is the authoritative source for this generation's logs.
    match saved_journal(inst, generation, usize::try_from(MAX_SAVED_LOG)?) {
        Ok(journal) => staged.write_all(&journal)?,
        Err(error) => {
            tracing::warn!("Cannot preserve stopped Firecracker journal: {error:#}");
            let unit = crate::firecracker_service::unit_for_generation(inst, generation)?;
            writeln!(
                staged,
                "Could not save the stopped Firecracker journal. Try `sudo journalctl -u {unit}` on the host."
            )?;
        }
    }
    // Rename over the link atomically so a killed cleanup never leaves the
    // instance without either the old link or the saved journal snapshot.
    staged
        .persist(&link)
        .map_err(|error| error.error)
        .context("Cannot publish stopped Firecracker log")?;
    Ok(())
}

fn saved_journal(inst: &Instance, generation: &str, limit: usize) -> Result<Vec<u8>> {
    let unit = crate::firecracker_service::unit_for_generation(inst, generation)?;
    let mut command = SetupTool::Journalctl
        .elevated_with_deadline(DEADLINE)?
        .args([
            "--no-pager",
            "--quiet",
            "--output=cat",
            "--unit",
            &unit,
            "--lines=1000",
        ])
        .build();
    let output = crate::process_output::run_bounded(
        &mut command,
        crate::process_output::CapturePolicy {
            stdout_max: limit,
            stderr_max: 4096,
            completion: crate::process_output::CompletionPolicy::Deadline(DEADLINE),
        },
    )
    .context("Cannot read a bounded Firecracker journal snapshot")?;
    if !output.status.success() {
        bail!("Firecracker journal snapshot command failed");
    }
    Ok(output.stdout)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn vmm_status_excludes_net_admin_from_capability_bounding_set() {
        let status = "Uid:\t123 123 123 123\nGid:\t123 123 123 123\nGroups:\nCapEff:\t0000000000000000\nCapBnd:\t0000000000000001\nNoNewPrivs:\t1\nSeccomp:\t2\n";
        verify_vmm_status(status, 123).unwrap();
        let unbounded = status.replace("CapBnd:\t0000000000000001", "CapBnd:\t0000000000001001");
        assert!(verify_vmm_status(&unbounded, 123).is_err());
        assert!(
            verify_vmm_status(
                &status.replace("CapBnd:\t0000000000000001", "CapBnd:\tbogus"),
                123
            )
            .is_err()
        );
        assert!(verify_vmm_status(&status.replace("Seccomp:\t2", "Seccomp:\t0"), 123).is_err());
    }
    use crate::config::{FirecrackerControlMode, ImageName, InstanceIndex, InstanceName};

    #[test]
    fn jailed_log_target_is_scoped_to_the_instance() {
        let root = crate::test_support::private_tempdir().unwrap();
        let inst = Instance {
            name: InstanceName::new("jail-test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: root.path().to_path_buf(),
            image: ImageName::new("default").unwrap(),
            firecracker_control: FirecrackerControlMode::Vsock,
        };
        let digest = Sha256Hash::of(root.path().as_os_str().as_bytes()).to_string();
        let owner = unsafe { libc::geteuid() }; // SAFETY: read-only credential query.
        let target = Path::new(JAIL_ROOT)
            .join(owner.to_string())
            .join("firecracker")
            .join(format!("{}-{}", &digest[..16], "0".repeat(32)))
            .join("root/firecracker.log");
        assert_eq!(checked_log_target(&inst, &target).unwrap(), target);
        assert!(checked_log_target(&inst, &target.with_file_name("vsock.sock")).is_err());
        let wrong_root = target
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("elsewhere/firecracker.log");
        assert!(checked_log_target(&inst, &wrong_root).is_err());
        assert!(checked_log_target(&inst, &target.with_file_name("../firecracker.log")).is_err());
        let short_generation = target
            .to_string_lossy()
            .replace(&"0".repeat(32), &"0".repeat(31));
        assert!(checked_log_target(&inst, Path::new(&short_generation)).is_err());
        let invalid_generation = target
            .to_string_lossy()
            .replace(&"0".repeat(32), &format!("{}g", "0".repeat(31)));
        assert!(checked_log_target(&inst, Path::new(&invalid_generation)).is_err());
        let uppercase_generation = target
            .to_string_lossy()
            .replace(&"0".repeat(32), &format!("{}A", "0".repeat(31)));
        assert!(checked_log_target(&inst, Path::new(&uppercase_generation)).is_err());
        let foreign = target
            .to_string_lossy()
            .replace(&digest[..16], "ffffffffffffffff");
        assert!(checked_log_target(&inst, Path::new(&foreign)).is_err());
    }

    #[test]
    fn candidate_uid_is_stable_and_uses_the_unprivileged_range() {
        let digest = Sha256Hash::of("instance").to_string();
        let first = candidate_uid(&digest, 0).unwrap();
        assert_eq!(candidate_uid(&digest, 0).unwrap(), first);
        assert_ne!(candidate_uid(&digest, 1).unwrap(), first);
        assert!((200_000..2_000_200_000).contains(&first));
    }

    #[test]
    fn log_reader_rejects_a_fifo_without_waiting_for_a_writer() {
        let root = crate::test_support::private_tempdir().unwrap();
        let inst = Instance {
            name: InstanceName::new("jail-log-test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: root.path().to_path_buf(),
            image: ImageName::new("default").unwrap(),
            firecracker_control: FirecrackerControlMode::Vsock,
        };
        let path = inst.log_path();
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a NUL-terminated path in a private temporary directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let writer_path = path.clone();
        let delayed_writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(writer_path)
                .unwrap()
        });
        let started = std::time::Instant::now();
        assert!(open_regular_log(&path).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        delayed_writer.join().unwrap();
    }

    #[test]
    #[ignore = "requires passwordless sudo and host ACL support"]
    fn privileged_staging_smoke_test() {
        let root = crate::test_support::private_tempdir().unwrap();
        let inst = Instance {
            name: InstanceName::new("jail-stage-test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: root.path().to_path_buf(),
            image: ImageName::new("default").unwrap(),
            firecracker_control: FirecrackerControlMode::Vsock,
        };
        let kernel = root.path().join("kernel");
        fs::write(&kernel, b"kernel witness").unwrap();
        fs::write(inst.rootfs_path(), b"disk witness").unwrap();
        let digest = Sha256Hash::of(root.path().as_os_str().as_bytes());
        let owner = unsafe { libc::geteuid() }; // SAFETY: read-only credential query.
        let unit = format!("coop-vmm-{owner}-{digest}-{}.service", "a".repeat(32));
        let jail = PreparedJail::new(&inst, &unit, &kernel).unwrap();
        let staged = jail.prepare(&inst, b"{\"boot-source\":{}}");
        let evidence = staged.is_ok().then(|| {
            (
                fs::read(jail.root.join("kernel")).unwrap(),
                fs::read(jail.root.join("config.json")).unwrap(),
                fs::read_link(inst.log_path()).unwrap(),
                fs::metadata(jail.root.join("firecracker")).unwrap(),
            )
        });
        jail.cleanup_unstarted(&inst).unwrap();
        release_identity(&inst).unwrap();
        staged.unwrap();
        let (kernel_bytes, config_bytes, log, executable) = evidence.unwrap();
        assert_eq!(kernel_bytes, b"kernel witness");
        assert_eq!(config_bytes, b"{\"boot-source\":{}}");
        assert_eq!(log, jail.root.join("firecracker.log"));
        assert_eq!(executable.mode() & 0o777, 0o700);
        assert_eq!(executable.len(), 0);
        assert!(!jail.root.exists());
        assert!(!inst.vsock_path().exists());
    }

    #[test]
    #[ignore = "requires passwordless sudo and host ACL support"]
    fn privileged_crash_recovery_reaps_jail_and_preserves_log() {
        let root = crate::test_support::private_tempdir().unwrap();
        let inst = Instance {
            name: InstanceName::new("jail-recovery-test").unwrap(),
            index: InstanceIndex::new(1).unwrap(),
            dir: root.path().to_path_buf(),
            image: ImageName::new("default").unwrap(),
            firecracker_control: FirecrackerControlMode::Vsock,
        };
        let kernel = root.path().join("kernel");
        fs::write(&kernel, b"kernel witness").unwrap();
        fs::write(inst.rootfs_path(), b"disk witness").unwrap();
        let digest = Sha256Hash::of(root.path().as_os_str().as_bytes());
        let owner = unsafe { libc::geteuid() }; // SAFETY: read-only credential query.
        let unit = format!("coop-vmm-{owner}-{digest}-{}.service", "b".repeat(32));
        let jail = PreparedJail::new(&inst, &unit, &kernel).unwrap();
        jail.prepare(&inst, b"{}").unwrap();
        // A stopped guest may replace its writable log with an arbitrary
        // object. Recovery must still reap the jail and revoke disk access.
        SetupTool::Rm
            .elevated_with_deadline(DEADLINE)
            .unwrap()
            .args(["-f", "--"])
            .arg(jail.root.join("firecracker.log"))
            .run_bounded(DEADLINE)
            .unwrap();
        SetupTool::Ln
            .elevated_with_deadline(DEADLINE)
            .unwrap()
            .args(["-s", "--", "/etc/passwd"])
            .arg(jail.root.join("firecracker.log"))
            .run_bounded(DEADLINE)
            .unwrap();
        cleanup_stopped(&inst).unwrap();
        SetupTool::Setfacl
            .elevated_with_deadline(DEADLINE)
            .unwrap()
            .args(["-m", &format!("u:{}:rw", jail.uid), "--"])
            .arg(inst.rootfs_path())
            .run_bounded(DEADLINE)
            .unwrap();
        cleanup_stopped(&inst).unwrap();
        let acl = std::process::Command::new("getfacl")
            .args(["-cp", "--"])
            .arg(inst.rootfs_path())
            .output()
            .unwrap();
        assert!(acl.status.success());
        assert!(!String::from_utf8_lossy(&acl.stdout).contains(&format!("user:{}:", jail.uid)));
        crate::vm::cleanup_runtime_paths(&inst).unwrap();
        release_identity(&inst).unwrap();
        assert!(!jail.root.exists());
        assert!(fs::symlink_metadata(inst.vsock_path()).is_err());
        assert!(
            fs::symlink_metadata(inst.log_path())
                .unwrap()
                .file_type()
                .is_file()
        );
        assert_eq!(fs::read(inst.log_path()).unwrap(), b"");
        assert_eq!(fs::read(inst.rootfs_path()).unwrap(), b"disk witness");
    }
}
