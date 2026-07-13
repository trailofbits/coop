use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::marker::PhantomData;
use std::net::TcpStream;
use std::num::NonZeroU8;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::backend::LogMode;
use crate::cmd::Cmd;
use crate::config::{CoopConfig, Instance, MiB};

// ── Typestate markers ─────────────────────────────────────────

/// VM has been constructed but not yet started.
pub struct Configured;
/// VM is running (either just started or attached via `from_running`).
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
        fs::create_dir_all(&self.inst.dir).context("Failed to create instance directory")?;

        let config_path = self.inst.vm_config_path();
        let mut fc_config = build_config(self.cfg, self.inst);
        if let Some(existing) = read_machine_config(&config_path) {
            fc_config.machine_config = existing;
        }
        let config_json = serde_json::to_string_pretty(&fc_config)
            .context("Failed to serialize Firecracker config")?;
        fs::write(&config_path, config_json).context("Failed to write Firecracker config")?;

        tracing::debug!("Wrote VM config to {}", config_path.display());
        Ok(())
    }

    /// Start the Firecracker process, consuming the `Configured`
    /// state and returning a `Running` VM.
    pub fn start(self) -> Result<FirecrackerVm<'a, Running>> {
        let config_path = self.inst.vm_config_path();
        let log_path = self.inst.log_path();
        let socket_path = self.inst.api_socket_path();

        // Kill orphaned Firecracker process if socket exists but no
        // PID file (crash between spawn and PID write).
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

        let fc_cmd = Cmd::new(&self.cfg.firecracker_bin)
            .arg("--api-sock")
            .arg(&socket_path)
            .arg("--config-file")
            .arg(&config_path)
            .arg("--log-path")
            .arg(&log_path)
            .args(["--level", "Info"])
            .sudo();
        let child = fc_cmd
            .build()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context(
                "Failed to start Firecracker — \
                 is it installed and in PATH?",
            )?;

        let pid = child.id();
        fs::write(self.inst.pid_file_path(), pid.to_string())
            .context("Failed to write PID file")?;

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

        // Give Firecracker a moment to start, then check it
        // didn't crash
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
    /// Attach to an already-running Firecracker VM by reading the
    /// PID file.
    ///
    /// Validates that the PID is alive and belongs to a Firecracker
    /// process, cleaning up stale PID files if not. Callers that
    /// already hold a `RunningInstance` proof should use
    /// [`Self::from_running_unchecked`] instead to avoid the
    /// redundant live-state probe.
    pub fn from_running(cfg: &'a CoopConfig, inst: &'a Instance) -> Result<Self> {
        if !inst.is_running() {
            let pid_path = inst.pid_file_path();
            bail!(
                "No running VM found for instance '{}' \
                 (no PID file at {})",
                inst.name,
                pid_path.display()
            );
        }

        Ok(Self::from_running_unchecked(cfg, inst))
    }

    /// Attach to a Firecracker VM whose running state has already
    /// been established by the caller (e.g. via a
    /// [`crate::backend::RunningInstance`] proof). Skips the
    /// `is_running()` probe that [`Self::from_running`] performs.
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

    /// Stop the Firecracker VM by sending a shutdown request via
    /// the API socket, falling back to SIGTERM then SIGKILL.
    pub fn stop(self) -> Result<()> {
        let pid_path = self.inst.pid_file_path();
        let pid_str = fs::read_to_string(&pid_path).context("Failed to read PID file")?;
        let pid: u32 = pid_str.trim().parse().context("Invalid PID")?;

        // Try graceful shutdown via SendCtrlAltDel action
        let socket_path = self.inst.api_socket_path();
        if socket_path.exists() {
            tracing::debug!("Sending CtrlAltDel via API socket");
            let result = Cmd::new("curl")
                .arg("--unix-socket")
                .arg(&socket_path)
                .args([
                    "-X",
                    "PUT",
                    "http://localhost/actions",
                    "-H",
                    "Content-Type: application/json",
                    "-d",
                    r#"{"action_type": "SendCtrlAltDel"}"#,
                ])
                .sudo()
                .output();

            if result.is_ok() {
                std::thread::sleep(Duration::from_secs(3));
            }
        }

        // Send SIGTERM as fallback (process is owned by root)
        if let Err(e) = Cmd::new("kill").arg(pid.to_string()).sudo().run() {
            tracing::debug!(
                "Failed to send SIGTERM to PID {pid} \
                 (non-fatal): {e}"
            );
        }

        // Wait for process to exit after SIGTERM
        let exited = wait_for_exit(pid, Duration::from_secs(10));

        if !exited {
            tracing::warn!(
                "Firecracker PID {pid} did not exit \
                 after SIGTERM, sending SIGKILL"
            );
            if let Err(e) = Cmd::new("kill").args(["-9", &pid.to_string()]).sudo().run() {
                tracing::debug!(
                    "Failed to send SIGKILL to PID {pid} \
                     (non-fatal): {e}"
                );
            }

            if !wait_for_exit(pid, Duration::from_secs(5)) {
                tracing::error!(
                    "Firecracker PID {pid} did not exit \
                     after SIGKILL"
                );
            }
        }

        // Clean up PID file and socket (socket owned by root)
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
/// orphaned Firecracker processes left behind when the PID file
/// was never written (crash between spawn and PID write).
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

/// Poll `kill -0` until the process exits or the timeout elapses.
/// Returns `true` if the process exited within the timeout.
fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    let pid_str = pid.to_string();
    while start.elapsed() < timeout {
        if !Cmd::new("kill").args(["-0", &pid_str]).sudo().status_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {
    use super::{MachineConfig, MiB, NonZeroU8, apply_machine_resources, read_machine_config};

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
}
