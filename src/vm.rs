use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::marker::PhantomData;
use std::net::TcpStream;
use std::num::NonZeroU8;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::backend::LogMode;
use crate::cmd::Cmd;
use crate::config::{CoopConfig, Instance, MiB};

// ── Typestate markers ─────────────────────────────────────────

/// VM has been constructed but not yet started.
pub struct Configured;
/// VM was started or observed running by its caller.
pub struct Running;

/// Represents a Firecracker VM instance.
///
/// The type parameter `S` encodes the VM lifecycle state:
/// - `Configured`: can call `configure()` and `start()`
/// - `Running`: can call `stop()`, `status()`, `stream_logs()`,
///   `wait_for_boot()`
///
/// Invalid transitions (e.g. stopping a configured-but-not-started VM)
/// are compile errors.
pub struct FirecrackerVm<'a, S> {
    cfg: &'a CoopConfig,
    inst: &'a Instance,
    _state: PhantomData<S>,
}

#[derive(Serialize, Deserialize)]
struct FirecrackerConfig {
    #[serde(rename = "boot-source")]
    boot_source: BootSource,
    drives: Vec<Drive>,
    #[serde(rename = "machine-config")]
    machine_config: MachineConfig,
    #[serde(rename = "network-interfaces")]
    network_interfaces: Vec<NetworkInterface>,
    vsock: Option<VsockConfig>,
}

#[derive(Serialize, Deserialize)]
struct BootSource {
    kernel_image_path: String,
    boot_args: String,
}

#[derive(Serialize, Deserialize)]
struct Drive {
    #[serde(rename = "drive_id")]
    id: String,
    path_on_host: String,
    is_root_device: bool,
    is_read_only: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct MachineConfig {
    vcpu_count: u8,
    mem_size_mib: u32,
}

#[derive(Serialize, Deserialize)]
struct NetworkInterface {
    iface_id: String,
    guest_mac: String,
    host_dev_name: String,
}

#[derive(Serialize, Deserialize)]
struct VsockConfig {
    guest_cid: u32,
    uds_path: String,
}

fn build_config(cfg: &CoopConfig, inst: &Instance) -> FirecrackerConfig {
    FirecrackerConfig {
        boot_source: BootSource {
            kernel_image_path: cfg.vm.kernel_path.display().to_string(),
            boot_args: cfg.vm.boot_args.clone(),
        },
        drives: vec![Drive {
            id: "rootfs".to_string(),
            path_on_host: inst.rootfs_path().display().to_string(),
            is_root_device: true,
            is_read_only: false,
        }],
        machine_config: MachineConfig {
            vcpu_count: cfg.vm.vcpu_count.get(),
            mem_size_mib: cfg.vm.mem_size_mib.get().as_u32(),
        },
        network_interfaces: vec![NetworkInterface {
            iface_id: "eth0".to_string(),
            guest_mac: inst.guest_mac(),
            host_dev_name: inst.tap_device(),
        }],
        vsock: Some(VsockConfig {
            guest_cid: inst.vsock_cid(),
            uds_path: inst.vsock_path().display().to_string(),
        }),
    }
}

/// Read the persisted machine config (mem/vcpu) from an instance's JSON.
///
/// Returns `None` if the file is missing or unparsable, letting callers
/// fall back to the global config for a not-yet-created instance.
fn read_machine_config(path: &Path) -> Option<MachineConfig> {
    let json = fs::read_to_string(path).ok()?;
    let config: FirecrackerConfig = serde_json::from_str(&json).ok()?;
    Some(config.machine_config)
}

/// The instance's currently persisted memory and vCPU count, as the typed
/// values `set_machine_resources` accepts.
///
/// Used to snapshot the prior values before a reconfigure so the caller
/// can roll them back if a subsequent boot fails, leaving the instance
/// bootable at its previous spec rather than wedged at the rejected one.
pub fn machine_resources(inst: &Instance) -> Result<(MiB, NonZeroU8)> {
    let path = inst.vm_config_path();
    let machine = read_machine_config(&path)
        .with_context(|| format!("Failed to read machine config from {}", path.display()))?;
    let mem = MiB::new(machine.mem_size_mib)
        .with_context(|| format!("Corrupt mem_size_mib=0 in {}", path.display()))?;
    let vcpus = NonZeroU8::new(machine.vcpu_count)
        .with_context(|| format!("Corrupt vcpu_count=0 in {}", path.display()))?;
    Ok((mem, vcpus))
}

/// Overwrite the memory and/or vCPU fields left `Some`, in place.
///
/// Split out from [`set_machine_resources`] so the field-selection logic
/// is unit-testable without touching the filesystem.
fn apply_machine_resources(
    machine: &mut MachineConfig,
    mem: Option<MiB>,
    vcpus: Option<NonZeroU8>,
) {
    if let Some(mem) = mem {
        machine.mem_size_mib = mem.as_u32();
    }
    if let Some(vcpus) = vcpus {
        machine.vcpu_count = vcpus.get();
    }
}

/// Update a stopped instance's persisted mem/vcpu, writing atomically so
/// a crash mid-write leaves the prior values intact.
///
/// The change takes effect on the next `coop start` (Firecracker re-reads
/// the JSON on boot); [`FirecrackerVm::configure`] preserves these fields
/// rather than resetting them to the global default.
pub fn set_machine_resources(
    inst: &Instance,
    mem: Option<MiB>,
    vcpus: Option<NonZeroU8>,
) -> Result<()> {
    let path = inst.vm_config_path();
    let json = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read VM config {}", path.display()))?;
    let mut config: FirecrackerConfig = serde_json::from_str(&json)
        .with_context(|| format!("Failed to parse VM config {}", path.display()))?;
    apply_machine_resources(&mut config.machine_config, mem, vcpus);
    let out =
        serde_json::to_string_pretty(&config).context("Failed to serialize Firecracker config")?;
    crate::fs_util::atomic_write_json(&path, &out)?;
    tracing::info!(
        "Updated machine config for instance '{}': {} vCPUs, {} MiB",
        inst.name,
        config.machine_config.vcpu_count,
        config.machine_config.mem_size_mib,
    );
    Ok(())
}

// ── Configured state ──────────────────────────────────────────

impl<'a> FirecrackerVm<'a, Configured> {
    pub fn new(cfg: &'a CoopConfig, inst: &'a Instance) -> Self {
        Self {
            cfg,
            inst,
            _state: PhantomData,
        }
    }

    /// Write the Firecracker JSON config.
    ///
    /// Infra fields (kernel path, boot args, drive, network, vsock) are
    /// regenerated from the global config every time so they roll forward
    /// on restart (e.g. after a kernel upgrade). Machine resources
    /// (mem/vcpu) are the exception: once an instance's config exists on
    /// disk it is authoritative for those two fields, so a value set via
    /// `coop resize` survives a restart instead of being reset to the
    /// global default. A fresh instance (no config yet) seeds them from
    /// the global config.
    pub fn configure(&self) -> Result<()> {
        crate::fs_util::private_dir(&self.inst.dir)
            .context("Failed to create instance directory")?;

        let config_path = self.inst.vm_config_path();
        let mut fc_config = build_config(self.cfg, self.inst);
        if let Some(existing) = read_machine_config(&config_path) {
            fc_config.machine_config = existing;
        }
        let config_json = serde_json::to_string_pretty(&fc_config)
            .context("Failed to serialize Firecracker config")?;
        crate::fs_util::atomic_write_json(&config_path, &config_json)
            .context("Failed to write Firecracker config")?;

        tracing::debug!("Wrote VM config to {}", config_path.display());
        Ok(())
    }

    /// Start the Firecracker process, consuming the `Configured`
    /// state and returning a `Running` VM.
    pub fn start(self) -> Result<FirecrackerVm<'a, Running>> {
        let config_path = self.inst.vm_config_path();
        let log_path = self.inst.log_path();
        let socket_path = self.inst.api_socket_path();

        // Kill an orphaned Firecracker if the socket is present without a
        // PID file: a prior stale-PID cleanup or interrupted startup can leave
        // the socket behind while its process is still alive.
        let pid_path = self.inst.pid_file_path();
        if socket_path.exists() && !pid_path.exists() {
            tracing::warn!(
                "Socket {} exists without PID file — killing orphaned process",
                socket_path.display()
            );
            kill_process_on_socket(&socket_path);
        }

        // Remove stale files from previous runs (may be owned by root)
        for stale in [&socket_path, &self.inst.vsock_path(), &log_path, &pid_path] {
            if stale.exists()
                && let Err(e) = Cmd::new("rm").arg("-f").arg(stale).sudo().run()
            {
                tracing::debug!(
                    "Failed to remove stale file {} (non-fatal): {e}",
                    stale.display()
                );
            }
        }

        // Launched through PID_TRAMPOLINE so the recorded PID is the VMM
        // rather than sudo's wrapper. Paths ride as positional argv ($1,
        // $@), so nothing is interpolated into the shell string.
        let fc_cmd = Cmd::new("sh")
            .args(["-c", PID_TRAMPOLINE, "sh"])
            .arg(&pid_path)
            .arg(&self.cfg.firecracker_bin)
            .arg("--api-sock")
            .arg(&socket_path)
            .arg("--config-file")
            .arg(&config_path)
            .arg("--log-path")
            .arg(&log_path)
            .args(["--level", "Info"])
            .sudo();
        let _child = fc_cmd
            .build()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context(
                "Failed to start Firecracker — \
                 is it installed and in PATH?",
            )?;

        let pid = wait_for_pid_file(&pid_path, Duration::from_secs(5))?;
        tracing::info!(
            "Firecracker started with PID {pid} \
             (instance '{}')",
            self.inst.name
        );

        let running = FirecrackerVm {
            cfg: self.cfg,
            inst: self.inst,
            _state: PhantomData,
        };

        // The PID file only proves the trampoline reached its exec, so
        // give Firecracker a beat to die before probing — otherwise
        // check_alive() races an immediate crash.
        std::thread::sleep(Duration::from_secs(1));
        running.check_alive().context(
            "Firecracker exited immediately — \
             check logs with `coop logs`",
        )?;

        Ok(running)
    }
}

// ── Running state ─────────────────────────────────────────────

impl<'a> FirecrackerVm<'a, Running> {
    /// Attach to a Firecracker VM whose running state has already
    /// been observed by the caller. The observation may become stale;
    /// lifecycle callers must serialize and recheck before mutation.
    pub fn from_running_unchecked(cfg: &'a CoopConfig, inst: &'a Instance) -> Self {
        Self {
            cfg,
            inst,
            _state: PhantomData,
        }
    }

    /// Wait for the guest to become reachable via SSH.
    pub fn wait_for_boot(&self) -> Result<()> {
        let addr = format!("{}:{}", self.inst.guest_ip(), self.cfg.ssh_port);
        let timeout = Duration::from_secs(60);
        let start = Instant::now();

        tracing::info!("Waiting for guest to boot (timeout: {timeout:?})");

        while start.elapsed() < timeout {
            if let Err(e) = self.check_alive() {
                bail!("Firecracker crashed during boot: {e}");
            }
            if TcpStream::connect_timeout(
                &addr.parse().context("Invalid guest address")?,
                Duration::from_secs(2),
            )
            .is_ok()
            {
                tracing::info!("Guest SSH is reachable");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        bail!(
            "Timed out waiting for guest to boot \
             after {timeout:?}"
        );
    }

    /// Request guest shutdown, then fall back to SIGTERM and SIGKILL.
    pub fn stop(self) -> Result<()> {
        self.stop_with_timeouts(
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::from_secs(5),
        )
    }

    fn stop_with_timeouts(
        self,
        graceful_timeout: Duration,
        term_timeout: Duration,
        kill_timeout: Duration,
    ) -> Result<()> {
        self.stop_with_probe(graceful_timeout, term_timeout, kill_timeout, wait_for_exit)
    }

    fn stop_with_probe(
        self,
        graceful_timeout: Duration,
        term_timeout: Duration,
        kill_timeout: Duration,
        mut wait: impl FnMut(u32, Duration) -> Result<bool>,
    ) -> Result<()> {
        let pid_path = self.inst.pid_file_path();
        let pid_str = fs::read_to_string(&pid_path).context("Failed to read PID file")?;
        let pid: u32 = pid_str.trim().parse().context("Invalid PID")?;
        let pid_i32 = i32::try_from(pid).context("Firecracker PID is out of range")?;
        if pid_i32 <= 0 {
            bail!("Firecracker PID must be positive");
        }

        if !wait(pid, Duration::ZERO)?
            && !self.shutdown_gracefully(pid, graceful_timeout, &mut wait)?
        {
            terminate_firecracker(pid, term_timeout, kill_timeout, &mut wait)?;
        }

        if let Err(e) = fs::remove_file(&pid_path) {
            tracing::debug!("Failed to remove PID file (non-fatal): {e}");
        }
        if let Err(e) = Cmd::new("rm")
            .arg("-f")
            .arg(self.inst.api_socket_path())
            .sudo()
            .run()
        {
            tracing::debug!("Failed to remove API socket (non-fatal): {e}");
        }

        Ok(())
    }

    fn shutdown_gracefully(
        &self,
        pid: u32,
        timeout: Duration,
        wait: &mut impl FnMut(u32, Duration) -> Result<bool>,
    ) -> Result<bool> {
        let request = self.shutdown_command().and_then(|mut command| {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("Cannot launch guest shutdown SSH request")
        });
        let mut child = match request {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!("Cannot request graceful guest shutdown: {error:#}");
                return Ok(false);
            }
        };
        // A successful reboot can disconnect SSH before it reports success.
        let stopped = wait(pid, timeout);
        reap_shutdown_client(&mut child);
        stopped
    }

    fn shutdown_command(&self) -> Result<Command> {
        let user = crate::backend::persisted_guest_user(self.cfg, &self.inst.image);
        let session = crate::backend::SshSession {
            target: crate::backend::SshTarget {
                host: self.inst.guest_ip().into(),
                port: self.cfg.ssh_port,
                user: crate::backend::SshUser::new(user.as_str())?,
                key_path: self.cfg.ssh_key_path(),
            },
            env: crate::backend::EnvForward::default(),
        };
        // Firecracker exits on guest reboot; its CtrlAltDel API is x86-only.
        session.command(&[], "sudo -n /sbin/reboot")
    }

    /// Return a human-readable status string with resource usage.
    pub fn status(&self) -> Result<String> {
        let pid_str =
            fs::read_to_string(self.inst.pid_file_path()).context("Failed to read PID file")?;
        let pid = pid_str.trim();

        // Report the mem/vcpu the VM actually booted with, read from the
        // authoritative per-instance config, not the global default.
        let machine = read_machine_config(&self.inst.vm_config_path()).unwrap_or(MachineConfig {
            vcpu_count: self.cfg.vm.vcpu_count.get(),
            mem_size_mib: self.cfg.vm.mem_size_mib.get().as_u32(),
        });

        let mut out = format!(
            "Instance '{}' running (PID: {pid})\n  \
             vCPUs: {}\n  Memory: {} MiB\n  \
             Guest IP: {}\n  SSH: {}:{}",
            self.inst.name,
            machine.vcpu_count,
            machine.mem_size_mib,
            self.inst.guest_ip(),
            self.inst.guest_ip(),
            self.cfg.ssh_port,
        );

        let guest_user = crate::backend::persisted_guest_user(self.cfg, &self.inst.image);
        let target = crate::backend::SshTarget {
            host: crate::backend::Hostname::from(self.inst.guest_ip()),
            port: self.cfg.ssh_port,
            user: crate::backend::SshUser::new(guest_user.as_str())?,
            key_path: self.cfg.ssh_key_path(),
        };
        if let Some(usage) = crate::backend::query_resource_usage(&target) {
            let _ = write!(out, "\n  {usage}");
        }

        Ok(out)
    }

    /// Stream the Firecracker log file to stdout.
    pub fn stream_logs(&self, mode: LogMode) -> Result<()> {
        let log_path = self.inst.log_path();
        if !log_path.exists() {
            bail!("No log file found at {}", log_path.display());
        }

        match mode {
            LogMode::Follow => {
                let mut child = Command::new("tail")
                    .arg("-f")
                    .arg(&log_path)
                    .spawn()
                    .context("Failed to tail log file")?;
                child.wait().context("Log streaming interrupted")?;
            }
            LogMode::Snapshot => {
                let file = fs::File::open(&log_path).context("Failed to open log file")?;
                let reader = BufReader::new(file);
                for line in reader.lines() {
                    let line = line.context("Failed to read log line")?;
                    writeln!(std::io::stdout(), "{line}").context("Failed to write log line")?;
                }
            }
        }
        Ok(())
    }

    /// Verify the Firecracker process is still running.
    fn check_alive(&self) -> Result<()> {
        let pid_str =
            fs::read_to_string(self.inst.pid_file_path()).context("Failed to read PID file")?;
        let pid = pid_str.trim();
        if !Cmd::new("kill").args(["-0", pid]).sudo().status_ok() {
            let log = fs::read_to_string(self.inst.log_path()).unwrap_or_default();
            let last_lines: String = log
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "Firecracker process (PID {pid}) is not \
                 running.\nLog tail:\n{last_lines}"
            );
        }
        Ok(())
    }
}

/// Find and kill any process holding the given Unix socket.
///
/// Uses `lsof` to find the PID, then sends SIGKILL. This handles
/// orphaned Firecracker processes whose PID file was removed while
/// the process still held the socket.
fn kill_process_on_socket(socket_path: &std::path::Path) {
    let Ok(stdout) = Cmd::new("lsof").arg("-t").arg(socket_path).sudo().capture() else {
        return;
    };
    for pid_str in stdout.split_whitespace() {
        tracing::info!(
            "Killing orphaned process {pid_str} on {}",
            socket_path.display()
        );
        if let Err(e) = Cmd::new("kill").args(["-9", pid_str]).sudo().run() {
            tracing::debug!("Failed to kill PID {pid_str} (non-fatal): {e}");
        }
    }
}

/// Shell wrapper that makes the recorded PID Firecracker's own.
///
/// `sudo` forks rather than execs when `use_pty` is on (its default since
/// 1.9.14), so the spawned child is the wrapper and `Child::id()` names it
/// instead of the VMM — `stop()`'s SIGKILL would then hit sudo and leave
/// the VM running. This shell records its own PID and execs in place, so
/// the PID file names the process SIGKILL must reach.
///
/// A root-owned PID file must be readable by the unprivileged caller despite
/// root's umask or the instance directory's default ACL. Set the mode before
/// atomically publishing it, so startup never observes an unreadable file.
/// Staging beside the destination keeps the rename on the same filesystem.
const PID_TRAMPOLINE: &str = concat!(
    r#"rm -f -- "$1" || exit 1; "#,
    r#"pid_tmp=$(mktemp -- "$1.XXXXXX") || exit 1; "#,
    r#"trap 'rm -f -- "$pid_tmp"' EXIT; "#,
    r#"trap 'exit 1' HUP INT TERM; "#,
    r#"echo $$ > "$pid_tmp" || exit 1; "#,
    r#"chmod 644 "$pid_tmp" || exit 1; "#,
    r#"mv -f -- "$pid_tmp" "$1" || exit 1; "#,
    r#"trap - EXIT HUP INT TERM; "#,
    r#"shift; exec "$@""#,
);

/// Poll for the PID file until its contents parse as a PID.
///
/// Returns an error on timeout, or on the first read error that is
/// not `NotFound`.
fn wait_for_pid_file(pid_path: &Path, timeout: Duration) -> Result<u32> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        match fs::read_to_string(pid_path) {
            Ok(pid_str) => {
                if let Ok(pid) = pid_str.trim().parse() {
                    return Ok(pid);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("Failed to read PID file"),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!(
        "Timed out waiting for valid PID file at {}",
        pid_path.display()
    );
}

/// Poll process liveness until exit or timeout. EPERM means the process is
/// alive; ESRCH proves exit. Any other probe error leaves identity evidence.
fn wait_for_exit(pid: u32, timeout: Duration) -> Result<bool> {
    let start = Instant::now();
    let pid = i32::try_from(pid).context("Firecracker PID is out of range")?;
    if pid <= 0 {
        bail!("Firecracker PID must be positive");
    }
    loop {
        // SAFETY: signal 0 performs a liveness check without sending a signal.
        if unsafe { libc::kill(pid, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => return Ok(true),
                Some(libc::EPERM) => {}
                _ => {
                    return Err(error).with_context(|| {
                        format!("Cannot determine liveness of Firecracker PID {pid}")
                    });
                }
            }
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn reap_shutdown_client(child: &mut Child) {
    if let Err(error) = child.kill() {
        tracing::debug!("Cannot terminate guest shutdown SSH client: {error}");
    }
    if let Err(error) = child.wait() {
        tracing::warn!("Cannot reap guest shutdown SSH client: {error}");
    }
}

fn terminate_firecracker(
    pid: u32,
    term_timeout: Duration,
    kill_timeout: Duration,
    wait: &mut impl FnMut(u32, Duration) -> Result<bool>,
) -> Result<()> {
    tracing::warn!("Graceful shutdown did not stop Firecracker PID {pid}; sending SIGTERM");
    if let Err(error) = Cmd::new("kill").arg(pid.to_string()).sudo().run() {
        tracing::debug!("Failed to send SIGTERM to PID {pid}: {error}");
    }
    if wait(pid, term_timeout)? {
        return Ok(());
    }
    tracing::warn!("Firecracker PID {pid} did not exit after SIGTERM; sending SIGKILL");
    if let Err(error) = Cmd::new("kill").args(["-9", &pid.to_string()]).sudo().run() {
        tracing::debug!("Failed to send SIGKILL to PID {pid}: {error}");
    }
    anyhow::ensure!(
        wait(pid, kill_timeout)?,
        "Firecracker PID {pid} is still alive after SIGKILL; PID file and socket retained for retry"
    );
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {
    use super::{
        Duration, FirecrackerVm, Instant, MachineConfig, MiB, NonZeroU8, apply_machine_resources,
        read_machine_config, wait_for_exit, wait_for_pid_file,
    };
    use crate::config::{CoopConfig, ImageName, Instance, InstanceIndex, InstanceName};

    #[test]
    fn configure_creates_and_replaces_private_config_under_permissive_umask() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;
        const CHILD: &str = "COOP_VM_CONFIG_PRIVATE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "vm::tests::configure_creates_and_replaces_private_config_under_permissive_umask"])
                .env(CHILD, "1").status().unwrap().success());
            return;
        }
        // SAFETY: the fixture runs in a separate process with no other tests.
        unsafe {
            libc::umask(0);
        }
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let cfg = CoopConfig::default();
        let inst = Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: root.path().join("instance"),
            image: ImageName::new("default").unwrap(),
        };
        let vm = FirecrackerVm::new(&cfg, &inst);
        vm.configure().unwrap();
        let path = inst.vm_config_path();
        assert_eq!(
            fs::metadata(&inst.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            read_machine_config(&path).unwrap().vcpu_count,
            cfg.vm.vcpu_count.get()
        );
        fs::write(&path, SAMPLE_CONFIG_JSON).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        vm.configure().unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let resources = read_machine_config(&path).unwrap();
        assert_eq!(resources.vcpu_count, 6);
        assert_eq!(resources.mem_size_mib, 5120);
        let outside = root.path().join("outside");
        fs::write(&outside, "untouched").unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(vm.configure().is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "isolated child-process stop cases share one fixture"
    )]
    fn stop_retains_pid_when_signals_fail_or_probe_is_invalid() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;
        use std::process::Command;

        let Ok(mode) = std::env::var("COOP_TEST_STOP_MODE") else {
            for mode in [
                "signals_fail",
                "probe_invalid",
                "probe_out_of_range",
                "probe_failure",
                "already_exited",
            ] {
                let root = tempfile::tempdir().unwrap();
                let sudo = root.path().join("sudo");
                fs::write(
                    &sudo,
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$COOP_TEST_STOP_CALLS\"\nexit 1\n",
                )
                .unwrap();
                fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();
                let ssh = root.path().join("ssh");
                fs::write(&ssh, "#!/bin/sh\nexit 255\n").unwrap();
                fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "vm::tests::stop_retains_pid_when_signals_fail_or_probe_is_invalid",
                    ])
                    .env("COOP_TEST_STOP_MODE", mode)
                    .env("COOP_TEST_STOP_ROOT", root.path())
                    .env("COOP_TEST_STOP_CALLS", root.path().join("calls"))
                    .env(
                        "PATH",
                        format!(
                            "{}:{}",
                            root.path().display(),
                            std::env::var("PATH").unwrap_or_default()
                        ),
                    )
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{mode}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let calls = fs::read_to_string(root.path().join("calls")).unwrap_or_default();
                if matches!(
                    mode,
                    "probe_invalid" | "probe_out_of_range" | "probe_failure" | "already_exited"
                ) {
                    assert!(
                        !calls.contains("kill"),
                        "unconfirmed live PID reached kill: {calls}"
                    );
                } else {
                    assert!(calls.contains("kill"), "{mode}: {calls}");
                }
                if mode == "signals_fail" {
                    assert!(calls.contains("kill -9"), "{calls}");
                }
            }
            return;
        };

        let root = std::path::PathBuf::from(std::env::var("COOP_TEST_STOP_ROOT").unwrap());
        let inst = Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: root.join("instance"),
            image: ImageName::new("default").unwrap(),
        };
        fs::create_dir(&inst.dir).unwrap();
        let pid = match mode.as_str() {
            "signals_fail" | "probe_failure" => std::process::id(),
            "probe_invalid" => 0,
            "probe_out_of_range" => i32::MAX as u32 + 1,
            "already_exited" => {
                let mut child = Command::new("true").spawn().unwrap();
                let pid = child.id();
                child.wait().unwrap();
                pid
            }
            _ => unreachable!(),
        };
        fs::write(inst.pid_file_path(), pid.to_string()).unwrap();
        let cfg = CoopConfig::default();
        let vm = FirecrackerVm::from_running_unchecked(&cfg, &inst);
        let result = if mode == "probe_failure" {
            vm.stop_with_probe(Duration::ZERO, Duration::ZERO, Duration::ZERO, |_, _| {
                Err(anyhow::anyhow!("injected liveness probe failure"))
            })
        } else {
            vm.stop_with_timeouts(Duration::ZERO, Duration::ZERO, Duration::ZERO)
        };
        if mode == "already_exited" {
            assert!(result.is_ok(), "{result:?}");
            assert!(!inst.pid_file_path().exists());
        } else {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains(match mode.as_str() {
                    "signals_fail" => "still alive",
                    "probe_invalid" => "positive",
                    "probe_out_of_range" => "out of range",
                    "probe_failure" => "injected liveness probe failure",
                    _ => unreachable!(),
                }),
                "{error}"
            );
            assert_eq!(
                fs::read_to_string(inst.pid_file_path()).unwrap(),
                pid.to_string()
            );
        }
    }

    #[test]
    fn stop_bounds_guest_shutdown_and_reaps_ssh() {
        let Ok(mode) = std::env::var("COOP_TEST_SHUTDOWN_MODE") else {
            for mode in ["graceful", "refused", "hung", "missing", "probe_error"] {
                run_shutdown_fixture(mode);
            }
            return;
        };
        let root = std::path::PathBuf::from(std::env::var("COOP_TEST_SHUTDOWN_ROOT").unwrap());
        let inst = Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: root.join("instance"),
            image: ImageName::new("default").unwrap(),
        };
        std::fs::create_dir(&inst.dir).unwrap();
        let mut process = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::fs::write(inst.pid_file_path(), process.id().to_string()).unwrap();
        let mut cfg = CoopConfig {
            data_dir: crate::config::ConfigPath::new(root.join("data")),
            ..CoopConfig::default()
        };
        cfg.guest_env
            .insert("PATH".parse().unwrap(), "/guest-only".into());
        let vm = FirecrackerVm::from_running_unchecked(&cfg, &inst);
        let started = Instant::now();
        let result = vm.stop_with_probe(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            |_, timeout| probe_shutdown_fixture(&mut process, &root, &mode, timeout),
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "unbounded shutdown"
        );
        assert_shutdown_result(result, &inst, &mut process, &mode);
        assert_shutdown_fixture(&root, &mode);
    }

    fn assert_shutdown_result(
        result: anyhow::Result<()>,
        inst: &Instance,
        process: &mut std::process::Child,
        mode: &str,
    ) {
        if mode == "probe_error" {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("injected probe failure")
            );
            assert!(inst.pid_file_path().exists());
            assert!(process.try_wait().unwrap().is_none());
            process.kill().unwrap();
            process.wait().unwrap();
        } else {
            result.unwrap();
            assert!(!inst.pid_file_path().exists());
            assert!(process.try_wait().unwrap().is_some());
        }
    }

    const SHUTDOWN_SUDO: &str = concat!(
        "#!/bin/sh\nset -eu\n",
        "printf '%s\\n' \"$*\" >> \"$COOP_TEST_SHUTDOWN_ROOT/signals\"\n",
        "case $1 in\n",
        "kill) shift; kill \"$@\";;\n",
        "rm) shift; exec /bin/rm \"$@\";;\n",
        "*) exit 1;;\nesac\n",
    );

    const SHUTDOWN_SSH: &str = concat!(
        "#!/bin/sh\nset -eu\n",
        "printf '%s\\n' \"$$\" > \"$COOP_TEST_SHUTDOWN_ROOT/ssh-pid\"\n",
        "for arg do remote=$arg; done\n",
        "[ \"$remote\" = 'sudo -n /sbin/reboot' ] || exit 99\n",
        "case $COOP_TEST_SHUTDOWN_MODE in\n",
        "graceful) pid=$(/bin/cat \"$COOP_TEST_SHUTDOWN_ROOT/instance/firecracker.pid\"); ",
        "kill \"$pid\"; exit 255;;\n",
        "refused) exit 1;;\n",
        "*) exec /bin/sleep 30;;\nesac\n",
    );

    fn run_shutdown_fixture(mode: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let sudo = root.path().join("sudo");
        std::fs::write(&sudo, SHUTDOWN_SUDO).unwrap();
        std::fs::set_permissions(sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        if mode != "missing" {
            let ssh = root.path().join("ssh");
            std::fs::write(&ssh, SHUTDOWN_SSH).unwrap();
            std::fs::set_permissions(ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "vm::tests::stop_bounds_guest_shutdown_and_reaps_ssh",
            ])
            .env("COOP_TEST_SHUTDOWN_ROOT", root.path())
            .env("COOP_TEST_SHUTDOWN_MODE", mode)
            .env("PATH", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn probe_shutdown_fixture(
        process: &mut std::process::Child,
        root: &std::path::Path,
        mode: &str,
        timeout: Duration,
    ) -> anyhow::Result<bool> {
        let started = Instant::now();
        loop {
            if process.try_wait()?.is_some() {
                return Ok(true);
            }
            if mode == "probe_error" && !timeout.is_zero() && root.join("ssh-pid").exists() {
                anyhow::bail!("injected probe failure");
            }
            if started.elapsed() >= timeout {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_shutdown_fixture(root: &std::path::Path, mode: &str) {
        let signals = std::fs::read_to_string(root.join("signals")).unwrap_or_default();
        if mode == "graceful" || mode == "probe_error" {
            assert!(!signals.contains("kill"), "{mode}: {signals}");
        } else {
            assert!(signals.contains("kill "), "{mode}: {signals}");
            assert!(!signals.contains("kill -9"), "{mode}: {signals}");
        }
        if mode == "missing" {
            assert!(!root.join("ssh-pid").exists());
        } else {
            let pid = std::fs::read_to_string(root.join("ssh-pid")).unwrap();
            assert!(wait_for_exit(pid.trim().parse().unwrap(), Duration::ZERO).unwrap());
        }
    }

    #[test]
    fn wait_for_exit_distinguishes_alive_and_exited_processes() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert!(!wait_for_exit(child.id(), Duration::ZERO).unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(wait_for_exit(child.id(), Duration::ZERO).unwrap());
        assert!(wait_for_exit(0, Duration::ZERO).is_err());
    }

    const SAMPLE_CONFIG_JSON: &str = r#"{
        "boot-source": {"kernel_image_path": "/k", "boot_args": "console=ttyS0"},
        "drives": [{"drive_id": "rootfs", "path_on_host": "/r",
                    "is_root_device": true, "is_read_only": false}],
        "machine-config": {"vcpu_count": 6, "mem_size_mib": 5120},
        "network-interfaces": [{"iface_id": "eth0", "guest_mac": "06:00:AC:10:00:02",
                                "host_dev_name": "tap0"}],
        "vsock": {"guest_cid": 3, "uds_path": "/v"}
    }"#;

    #[test]
    fn read_machine_config_parses_persisted_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm_config.json");
        std::fs::write(&path, SAMPLE_CONFIG_JSON).unwrap();
        let machine = read_machine_config(&path).unwrap();
        assert_eq!(machine.vcpu_count, 6);
        assert_eq!(machine.mem_size_mib, 5120);
    }

    #[test]
    fn read_machine_config_returns_none_for_missing_or_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(read_machine_config(&missing).is_none(), "missing file");

        let malformed = dir.path().join("bad.json");
        std::fs::write(&malformed, "{ not json").unwrap();
        assert!(read_machine_config(&malformed).is_none(), "malformed file");
    }

    #[test]
    fn apply_machine_resources_updates_only_provided_fields() {
        let mut machine = MachineConfig {
            vcpu_count: 2,
            mem_size_mib: 2048,
        };

        apply_machine_resources(&mut machine, MiB::new(4096), None);
        assert_eq!(machine.mem_size_mib, 4096);
        assert_eq!(machine.vcpu_count, 2, "vcpu untouched when mem-only");

        apply_machine_resources(&mut machine, None, NonZeroU8::new(8));
        assert_eq!(machine.vcpu_count, 8);
        assert_eq!(machine.mem_size_mib, 4096, "mem untouched when vcpu-only");
    }

    #[test]
    fn apply_machine_resources_is_noop_when_both_none() {
        let mut machine = MachineConfig {
            vcpu_count: 2,
            mem_size_mib: 2048,
        };
        apply_machine_resources(&mut machine, None, None);
        assert_eq!(machine.vcpu_count, 2);
        assert_eq!(machine.mem_size_mib, 2048);
    }

    #[test]
    fn machine_config_round_trips_through_json() {
        let machine = MachineConfig {
            vcpu_count: 4,
            mem_size_mib: 3072,
        };
        let json = serde_json::to_string(&machine).unwrap();
        let back: MachineConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.vcpu_count, 4);
        assert_eq!(back.mem_size_mib, 3072);
    }

    #[test]
    fn pid_trampoline_publishes_readable_pid_under_hardened_umask() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.pid");
        // Pause any post-publication chmod in the shell itself. This makes the
        // old create-then-chmod window deterministic without sudo or a VM.
        // The replacement process also stays alive until the test reaps it.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "published=$1; umask 077; chmod() {{ if [ \"$2\" = \"$published\" ]; then kill -STOP $$; fi; command chmod \"$@\"; }}; {}",
                crate::vm::PID_TRAMPOLINE,
            ))
            .arg("sh")
            .arg(&path)
            .args(["sleep", "30"])
            .spawn()
            .unwrap();
        let published = wait_for_pid_file(&path, Duration::from_secs(5));
        let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777);
        let expected_pid = child.id();
        // Reap even when an assertion below fails, including the stopped shell.
        child.kill().unwrap();
        child.wait().unwrap();

        assert_eq!(published.unwrap(), expected_pid);
        assert_eq!(mode.unwrap(), 0o644);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pid_trampoline_normalizes_inherited_default_acl() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let directory = std::ffi::CString::new(dir.path().as_os_str().as_bytes()).unwrap();
        // Linux POSIX default ACL u::rwx,g::---,o::---. Such an ACL replaces
        // the umask during file creation; changing the umask cannot grant read.
        let mut acl = 2_u32.to_le_bytes().to_vec();
        for (tag, permissions) in [(1_u16, 7_u16), (4, 0), (32, 0)] {
            acl.extend(tag.to_le_bytes());
            acl.extend(permissions.to_le_bytes());
            acl.extend(u32::MAX.to_le_bytes());
        }
        // SAFETY: both C strings and the ACL buffer live through setxattr;
        // the buffer length describes its complete initialized contents.
        let status = unsafe {
            libc::setxattr(
                directory.as_ptr(),
                c"system.posix_acl_default".as_ptr(),
                acl.as_ptr().cast(),
                acl.len(),
                0,
            )
        };
        assert_eq!(
            status,
            0,
            "set default ACL: {}",
            std::io::Error::last_os_error()
        );

        // Positive witness that this filesystem applies the restrictive ACL.
        let witness = dir.path().join("inherited");
        std::fs::write(&witness, "fixture").unwrap();
        assert_eq!(
            std::fs::metadata(&witness).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let path = dir.path().join("firecracker.pid");
        let result = std::process::Command::new("sh")
            .args(["-c", crate::vm::PID_TRAMPOLINE, "sh"])
            .arg(&path)
            .arg("true")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap()
                > 0
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn pid_trampoline_cleans_staging_file_when_chmod_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.pid");
        let result = std::process::Command::new("sh")
            .args([
                "-c",
                &format!(
                    "chmod() {{ echo chmod-refused >&2; return 1; }}; {}",
                    crate::vm::PID_TRAMPOLINE
                ),
                "sh",
            ])
            .arg(&path)
            .arg("true")
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("chmod-refused"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn wait_for_pid_file_returns_immediately_when_already_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.pid");
        std::fs::write(&path, "4242\n").unwrap();

        let start = Instant::now();
        let pid = wait_for_pid_file(&path, Duration::from_secs(5)).unwrap();

        assert_eq!(pid, 4242);
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "a ready PID file must not cost a poll interval"
        );
    }

    #[test]
    fn wait_for_pid_file_retries_past_the_empty_write_window() {
        // The trampoline's `echo $$ > "$1"` truncates before it writes, so a
        // reader can legitimately observe a zero-byte file. That must retry,
        // not fail.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.pid");
        std::fs::write(&path, "").unwrap();

        let writer = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            std::fs::write(&writer, "777\n").unwrap();
        });

        assert_eq!(
            wait_for_pid_file(&path, Duration::from_secs(5)).unwrap(),
            777
        );
    }

    #[test]
    fn wait_for_pid_file_times_out_when_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-written.pid");

        let err = wait_for_pid_file(&path, Duration::from_millis(300)).unwrap_err();

        assert!(err.to_string().contains("Timed out"), "got: {err}");
    }

    #[test]
    fn wait_for_pid_file_propagates_errors_other_than_not_found() {
        // A directory reads as EISDIR, not NotFound: abort rather than spin
        // until the timeout. This is the arm an unreadable root-owned PID
        // file also takes.
        let dir = tempfile::tempdir().unwrap();

        let start = Instant::now();
        let err = wait_for_pid_file(dir.path(), Duration::from_secs(30)).unwrap_err();

        assert!(
            start.elapsed() < Duration::from_secs(5),
            "a non-NotFound error must return at once, not poll to timeout"
        );
        assert!(
            err.to_string().contains("Failed to read PID file"),
            "got: {err}"
        );
    }
}
