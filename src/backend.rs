#[cfg(target_os = "macos")]
use std::fs;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use toml::Value as TomlValue;

#[cfg(not(target_os = "macos"))]
use crate::cmd::Cmd;
use crate::config::{ConfigDir, CoopConfig, GitHubAuth, Instance, McpServerDef};
use crate::setup::SetupOptions;
use crate::shell::shell_escape;

// ── Guest path newtype ────────────────────────────────────────

/// Path inside the guest VM. Prevents confusion between host
/// `PathBuf` and guest path strings (which use Linux conventions
/// regardless of the host OS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestPath(String);

impl GuestPath {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GuestPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Environment forwarding ────────────────────────────────────

/// Environment variables to forward to guest VMs via SSH `SendEnv`.
///
/// Carries both variable names (for `-o SendEnv=`) and their values
/// (for `Command::env()` on SSH child processes), avoiding unsafe
/// mutation of the process-global environment.
#[derive(Debug, Clone, Default)]
pub struct EnvForward {
    vars: IndexMap<String, String>,
}

impl EnvForward {
    /// Insert or overwrite an env var.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(name.into(), value.into());
    }

    /// Check whether a variable is present.
    pub fn contains(&self, name: &str) -> bool {
        self.vars.contains_key(name)
    }

    /// SSH `-o SendEnv=` args for all contained variables.
    fn send_env_opts(&self) -> Vec<String> {
        let mut opts = Vec::with_capacity(self.vars.len() * 2);
        for name in self.vars.keys() {
            opts.push("-o".into());
            opts.push(format!("SendEnv={name}"));
        }
        opts
    }

    /// Key-value pairs for `Command::envs()`.
    pub fn as_envs(&self) -> &IndexMap<String, String> {
        &self.vars
    }
}

// ── Running instance ──────────────────────────────────────────

/// Instance that has been verified as running. Carries the SSH
/// target so connection details are always available without
/// further fallible lookups.
pub struct RunningInstance {
    pub inst: Instance,
    pub target: SshTarget,
}

// ── SSH target ────────────────────────────────────────────────

/// SSH connection details for reaching a guest VM.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub port: NonZeroU16,
    pub user: String,
    pub key_path: PathBuf,
}

// ── SSH session ───────────────────────────────────────────────

/// SSH operations that combine connection details with environment
/// forwarding. Borrows both to avoid duplication at call sites
/// where target and env are always passed together.
pub struct SshSession<'a> {
    pub target: &'a SshTarget,
    pub env: &'a EnvForward,
}

impl SshTarget {
    /// Control socket path for SSH connection multiplexing.
    ///
    /// Uses `XDG_RUNTIME_DIR` (per-user, 0700 on Linux) when available,
    /// falling back to `std::env::temp_dir()` (per-user `$TMPDIR` on
    /// macOS, `/tmp` on Linux).
    ///
    /// The filename is kept short because SSH creates a temp socket
    /// with an 18-char random suffix during setup. macOS limits Unix
    /// domain socket paths to 104 chars, so the full path (including
    /// the temp suffix) must stay under that limit.
    fn control_path(&self) -> PathBuf {
        let dir =
            std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        // Hash host:port to keep the filename short and predictable.
        // 8 hex chars (32 bits) is enough to avoid collisions across
        // the handful of concurrent instances this tool manages.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&(&self.host, self.port), &mut hasher);
        let hash = std::hash::Hasher::finish(&hasher);
        // Truncation to 32 bits is intentional — we only need enough
        // uniqueness to distinguish a handful of concurrent instances.
        #[expect(clippy::cast_possible_truncation)]
        let short = hash as u32;
        dir.join(format!("coop-{short:08x}.sock"))
    }

    /// SSH options for commands.
    pub fn ssh_opts(&self) -> Vec<String> {
        vec![
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-i".into(),
            self.key_path.display().to_string(),
            "-p".into(),
            self.port.to_string(),
        ]
    }

    /// SSH options with connection multiplexing.
    ///
    /// Only used during boot probing (`wait_until_ready`) where rapid
    /// retries benefit from a shared master connection. The master is
    /// torn down after probing to avoid poisoning later connections
    /// that need `SendEnv` for environment forwarding.
    fn ssh_opts_mux(&self) -> Vec<String> {
        let mut opts = self.ssh_opts();
        opts.extend([
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", self.control_path().display()),
            "-o".into(),
            "ControlPersist=60".into(),
        ]);
        opts
    }

    /// SCP options (uses -P for port instead of -p).
    pub fn scp_opts(&self) -> Vec<String> {
        vec![
            "-q".into(),
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-i".into(),
            self.key_path.display().to_string(),
            "-P".into(),
            self.port.to_string(),
        ]
    }

    /// user@host address string.
    pub fn addr(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// Run a command on the guest via SSH.
    pub fn exec(&self, cmd: &str) -> Result<()> {
        let mut args = self.ssh_opts();
        args.push(self.addr());
        args.push(cmd.to_string());

        let status = Command::new("ssh")
            .args(&args)
            .status()
            .context("Failed to run SSH command")?;

        if !status.success() {
            bail!("SSH command failed: {cmd}");
        }
        Ok(())
    }

    /// Check if a command succeeds on the guest.
    pub fn exec_ok(&self, cmd: &str) -> bool {
        let mut args = self.ssh_opts();
        args.push(self.addr());
        args.push(cmd.to_string());

        Command::new("ssh")
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Like `exec_ok` but uses connection multiplexing for fast retries.
    fn probe_ok_mux(&self, cmd: &str) -> bool {
        let mut args = self.ssh_opts_mux();
        args.push(self.addr());
        args.push(cmd.to_string());

        Command::new("ssh")
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Wait until SSH is ready, retrying with exponential backoff.
    ///
    /// TCP reachability (checked by `wait_for_boot`) does not guarantee
    /// sshd is accepting authentication. This probe retries a trivial
    /// command until it succeeds or the timeout expires.
    ///
    /// Uses a multiplexed connection for fast retries, then tears down
    /// the master so later connections negotiate their own `SendEnv`.
    pub fn wait_until_ready(&self, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        let mut delay = Duration::from_millis(250);
        let max_delay = Duration::from_secs(4);

        tracing::info!("Probing SSH readiness (timeout: {timeout:?})");

        loop {
            if self.probe_ok_mux("true") {
                tracing::info!("SSH is ready");
                self.close_mux();
                return Ok(());
            }

            if start.elapsed() >= timeout {
                self.close_mux();
                bail!(
                    "SSH not ready after {timeout:?} — \
                     sshd may not be running in the guest"
                );
            }

            tracing::debug!("SSH not ready yet, retrying in {delay:?}");
            std::thread::sleep(delay);
            delay = (delay * 2).min(max_delay);
        }
    }

    /// SCP a local file to the guest.
    pub fn scp_to(&self, local_path: &Path, remote: &GuestPath) -> Result<()> {
        let status = Command::new("scp")
            .args(self.scp_opts())
            .arg(local_path)
            .arg(format!("{}:{remote}", self.addr()))
            .status()
            .context("Failed to run scp")?;

        if !status.success() {
            bail!("scp failed: {} -> {remote}", local_path.display());
        }
        Ok(())
    }

    /// Copy a local directory to the guest recursively via scp.
    pub fn scp_to_recursive(&self, local_path: &Path, remote: &GuestPath) -> Result<()> {
        let status = Command::new("scp")
            .args(self.scp_opts())
            .arg("-r")
            .arg(local_path)
            .arg(format!("{}:{remote}", self.addr()))
            .status()
            .context("Failed to run scp")?;

        if !status.success() {
            bail!("scp -r failed: {} -> {remote}", local_path.display());
        }
        Ok(())
    }

    /// SSH command string for rsync's -e flag.
    pub fn rsync_ssh_cmd(&self) -> String {
        format!(
            "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
             -o LogLevel=ERROR -i {} -p {}",
            self.key_path.display(),
            self.port,
        )
    }

    /// Run a command on the guest via SSH and capture stdout.
    pub fn capture(&self, cmd: &str) -> Result<String> {
        let mut args = self.ssh_opts();
        args.push(self.addr());
        args.push(cmd.to_string());

        let output = Command::new("ssh")
            .args(&args)
            .stderr(std::process::Stdio::null())
            .output()
            .context("Failed to run SSH command")?;

        if !output.status.success() {
            bail!("SSH command failed: {cmd}");
        }
        String::from_utf8(output.stdout).context("SSH output is not valid UTF-8")
    }

    /// Tear down the SSH control master connection.
    ///
    /// Best-effort: logs failures but never errors. Safe to call
    /// even if no master is running.
    fn close_mux(&self) {
        let sock = self.control_path();
        if !sock.exists() {
            return;
        }
        tracing::debug!("Closing SSH control master at {}", sock.display());
        let control_arg = format!("ControlPath={}", sock.display());
        let result = Command::new("ssh")
            .args(["-O", "exit", "-o", &control_arg, &self.addr()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match result {
            Ok(s) if s.success() => {
                tracing::debug!("Closed SSH control master");
            }
            Ok(s) => {
                tracing::debug!("ssh -O exit exited with {s}");
            }
            Err(e) => {
                tracing::debug!("Failed to close SSH control master: {e}");
            }
        }
    }
}

impl SshSession<'_> {
    /// SSH options with environment variable forwarding.
    pub fn ssh_opts(&self) -> Vec<String> {
        let mut opts = self.target.ssh_opts();
        opts.extend(self.env.send_env_opts());
        opts
    }

    /// Run a command on the guest via SSH with env forwarding.
    pub fn exec(&self, cmd: &str) -> Result<()> {
        let mut args = self.ssh_opts();
        args.push(self.target.addr());
        args.push(cmd.to_string());

        let status = Command::new("ssh")
            .args(&args)
            .envs(self.env.as_envs())
            .status()
            .context("Failed to run SSH command")?;

        if !status.success() {
            bail!("SSH command failed: {cmd}");
        }
        Ok(())
    }
}

// ── Resource usage ────────────────────────────────────────────

/// Runtime resource usage gathered from a running guest VM.
pub struct ResourceUsage {
    /// 1-minute load average.
    pub load_1m: f64,
    /// Memory used in MiB.
    pub mem_used_mib: u64,
    /// Total memory in MiB.
    pub mem_total_mib: u64,
    /// Disk used in MiB on root filesystem.
    pub disk_used_mib: u64,
    /// Total disk in MiB on root filesystem.
    pub disk_total_mib: u64,
}

impl std::fmt::Display for ResourceUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mem_pct = if self.mem_total_mib > 0 {
            self.mem_used_mib * 100 / self.mem_total_mib
        } else {
            0
        };
        let disk_pct = if self.disk_total_mib > 0 {
            self.disk_used_mib * 100 / self.disk_total_mib
        } else {
            0
        };
        write!(
            f,
            "Load: {:.2}  Mem: {}/{} MiB ({mem_pct}%)  \
             Disk: {}/{} MiB ({disk_pct}%)",
            self.load_1m,
            self.mem_used_mib,
            self.mem_total_mib,
            self.disk_used_mib,
            self.disk_total_mib,
        )
    }
}

impl ResourceUsage {
    /// Compact single-line summary for multi-instance listing.
    pub fn summary(&self) -> String {
        let mem_pct = if self.mem_total_mib > 0 {
            self.mem_used_mib * 100 / self.mem_total_mib
        } else {
            0
        };
        let disk_pct = if self.disk_total_mib > 0 {
            self.disk_used_mib * 100 / self.disk_total_mib
        } else {
            0
        };
        format!("load={:.2} mem={mem_pct}% disk={disk_pct}%", self.load_1m)
    }
}

/// Query resource usage from a running guest via SSH.
///
/// Runs a single compound command to gather load average, memory,
/// and disk usage from `/proc` and `df`. Returns `None` if the
/// query fails (e.g. SSH not reachable).
pub fn query_resource_usage(target: &SshTarget) -> Option<ResourceUsage> {
    let cmd = "cat /proc/loadavg; cat /proc/meminfo; df -m /";
    let output = target.capture(cmd).ok()?;
    Some(parse_resource_usage(&output))
}

fn parse_resource_usage(output: &str) -> ResourceUsage {
    let mut load_1m = 0.0;
    let mut mem_total_kib: u64 = 0;
    let mut mem_available_kib: u64 = 0;
    let mut disk_used_mib: u64 = 0;
    let mut disk_total_mib: u64 = 0;

    for line in output.lines() {
        // /proc/loadavg: "0.12 0.08 0.03 1/42 1234"
        if let Some(first) = line.split_whitespace().next()
            && load_1m == 0.0
            && let Ok(v) = first.parse::<f64>()
        {
            load_1m = v;
            continue;
        }

        // /proc/meminfo: "MemTotal:       2048000 kB"
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            if let Some(val) = parse_meminfo_kib(rest) {
                mem_total_kib = val;
            }
        } else if let Some(rest) = line.strip_prefix("MemAvailable:")
            && let Some(val) = parse_meminfo_kib(rest)
        {
            mem_available_kib = val;
        }

        // df -m output: "/dev/vda1  20480  3200  16000  17% /"
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 4
            && let (Ok(total), Ok(used)) = (fields[1].parse::<u64>(), fields[2].parse::<u64>())
            && fields.last().is_some_and(|f| f.starts_with('/'))
        {
            disk_total_mib = total;
            disk_used_mib = used;
        }
    }

    let mem_total_mib = mem_total_kib / 1024;
    let mem_used_mib = mem_total_kib.saturating_sub(mem_available_kib) / 1024;

    ResourceUsage {
        load_1m,
        mem_used_mib,
        mem_total_mib,
        disk_used_mib,
        disk_total_mib,
    }
}

fn parse_meminfo_kib(value: &str) -> Option<u64> {
    value.split_whitespace().next()?.parse::<u64>().ok()
}

// ── Backend trait ──────────────────────────────────────────────

/// VM backend for managing guest lifecycle.
///
/// Two impls: `FirecrackerBackend` (Linux) and `LimaBackend` (macOS).
/// The `PlatformBackend` type alias selects the correct one at compile
/// time via `#[cfg]`.
pub trait VmBackend: std::fmt::Display {
    fn setup(&self, cfg: &CoopConfig, opts: &SetupOptions) -> Result<()>;
    fn create_and_start(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        disk_gib: Option<crate::config::GiB>,
        mounts: &[crate::config::Mount],
    ) -> Result<()>;
    fn start_existing(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()>;
    fn stop(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()>;
    fn destroy_instance(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()>;
    fn destroy_shared(&self, cfg: &CoopConfig);
    fn destroy_image(&self, cfg: &CoopConfig, image: &str) -> Result<()>;
    fn resize_disk(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        new_size: crate::config::GiB,
    ) -> Result<()>;
    fn is_running(&self, inst: &Instance) -> bool;
    fn status(&self, cfg: &CoopConfig, inst: &Instance) -> Result<String>;
    fn stream_logs(&self, cfg: &CoopConfig, inst: &Instance, follow: bool) -> Result<()>;
    fn ssh_target(&self, cfg: &CoopConfig, inst: &Instance) -> Result<SshTarget>;
    fn disk_path(&self, inst: &Instance) -> Result<PathBuf>;
    /// Whether mounts use live filesystem sharing (Lima/virtiofs)
    /// vs one-time sync (Firecracker/rsync).
    fn mounts_are_live(&self) -> bool;
}

// ── Firecracker backend ───────────────────────────────────────

#[cfg(not(target_os = "macos"))]
pub struct FirecrackerBackend;

#[cfg(not(target_os = "macos"))]
impl FirecrackerBackend {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(target_os = "macos"))]
impl std::fmt::Display for FirecrackerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("firecracker")
    }
}

#[cfg(not(target_os = "macos"))]
impl VmBackend for FirecrackerBackend {
    fn setup(&self, cfg: &CoopConfig, opts: &SetupOptions) -> Result<()> {
        crate::setup::run(cfg, opts)
    }

    fn create_and_start(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        disk_gib: Option<crate::config::GiB>,
        mounts: &[crate::config::Mount],
    ) -> Result<()> {
        // Mounts are handled after boot via rsync (not virtiofs).
        // Validation already happened in Mount::parse().
        let _ = mounts;
        crate::setup::create_instance(cfg, inst, disk_gib)?;
        let vm = crate::vm::FirecrackerVm::new(cfg, inst);
        vm.configure()?;
        crate::network::setup_tap(&cfg.network, inst)?;
        let running = vm.start()?;
        running.wait_for_boot()
    }

    fn start_existing(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        let vm = crate::vm::FirecrackerVm::new(cfg, inst);
        vm.configure()?;
        crate::network::setup_tap(&cfg.network, inst)?;
        let running = vm.start()?;
        running.wait_for_boot()
    }

    fn stop(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        if !self.is_running(inst) {
            tracing::debug!("Instance '{}' is not running — nothing to stop", inst.name);
            return Ok(());
        }
        let vm = crate::vm::FirecrackerVm::from_running(cfg, inst)?;
        vm.stop()
    }

    fn destroy_instance(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        if let Ok(vm) = crate::vm::FirecrackerVm::from_running(cfg, inst) {
            vm.stop()?;
        }
        crate::network::teardown_tap(&cfg.network, inst)?;
        if inst.dir.exists()
            && let Err(e) = Cmd::new("rm").args(["-rf"]).arg(&inst.dir).sudo().run()
        {
            tracing::debug!(
                "Failed to remove instance dir {} (non-fatal): {e}",
                inst.dir.display()
            );
        }
        Ok(())
    }

    fn destroy_shared(&self, cfg: &CoopConfig) {
        let images_dir = cfg.images_dir();
        if images_dir.exists()
            && let Err(e) = Cmd::new("rm").args(["-rf"]).arg(&images_dir).sudo().run()
        {
            tracing::debug!("Failed to remove images dir (non-fatal): {e}");
        }

        crate::network::teardown_all(&cfg.network);
        tracing::info!("Removing kernel and Firecracker binary");
        for path in [cfg.vm.kernel_path.clone(), cfg.firecracker_bin.clone()] {
            if path.exists()
                && let Err(e) = Cmd::new("rm").arg("-f").arg(&path).sudo().run()
            {
                tracing::debug!("Failed to remove {} (non-fatal): {e}", path.display());
            }
        }
        let jailer = cfg.data_dir.join("jailer");
        if jailer.exists()
            && let Err(e) = Cmd::new("rm").arg("-f").arg(&jailer).sudo().run()
        {
            tracing::debug!("Failed to remove jailer (non-fatal): {e}");
        }
    }

    fn destroy_image(&self, cfg: &CoopConfig, image: &str) -> Result<()> {
        let dir = cfg.image_dir(image);
        if !dir.exists() {
            bail!("Image '{image}' does not exist");
        }
        Cmd::new("rm")
            .args(["-rf"])
            .arg(&dir)
            .sudo()
            .run()
            .with_context(|| format!("Failed to remove image dir {}", dir.display()))?;
        tracing::info!("Removed image '{image}'");
        Ok(())
    }

    fn resize_disk(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        new_size: crate::config::GiB,
    ) -> Result<()> {
        if self.is_running(inst) {
            bail!(
                "Instance '{}' is running — stop it first with \
                 `coop stop {}`",
                inst.name,
                inst.name,
            );
        }
        let _ = cfg;
        crate::setup::resize_rootfs(inst, new_size)
    }

    fn is_running(&self, inst: &Instance) -> bool {
        inst.is_running()
    }

    fn status(&self, cfg: &CoopConfig, inst: &Instance) -> Result<String> {
        let vm = crate::vm::FirecrackerVm::from_running(cfg, inst)?;
        vm.status()
    }

    fn stream_logs(&self, cfg: &CoopConfig, inst: &Instance, follow: bool) -> Result<()> {
        let vm = crate::vm::FirecrackerVm::from_running(cfg, inst)?;
        vm.stream_logs(follow)
    }

    fn ssh_target(&self, cfg: &CoopConfig, inst: &Instance) -> Result<SshTarget> {
        Ok(SshTarget {
            host: inst.guest_ip(),
            port: cfg.ssh_port,
            user: crate::guest::GUEST_USER.to_string(),
            key_path: cfg.ssh_key_path(),
        })
    }

    fn disk_path(&self, inst: &Instance) -> Result<PathBuf> {
        Ok(inst.rootfs_path())
    }

    fn mounts_are_live(&self) -> bool {
        false
    }
}

// ── Lima backend ──────────────────────────────────────────────

#[cfg(target_os = "macos")]
pub struct LimaBackend;

#[cfg(target_os = "macos")]
impl LimaBackend {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "macos")]
impl std::fmt::Display for LimaBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lima")
    }
}

#[cfg(target_os = "macos")]
impl VmBackend for LimaBackend {
    fn setup(&self, cfg: &CoopConfig, opts: &SetupOptions) -> Result<()> {
        crate::lima::setup(cfg, opts)
    }

    fn create_and_start(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        disk_gib: Option<crate::config::GiB>,
        mounts: &[crate::config::Mount],
    ) -> Result<()> {
        crate::lima::create_and_start(cfg, inst, disk_gib, mounts)
    }

    fn start_existing(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        crate::lima::start_existing(cfg, inst)
    }

    fn stop(&self, _cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        crate::lima::stop(inst)
    }

    fn destroy_instance(&self, _cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        crate::lima::destroy(inst)?;
        if inst.dir.exists()
            && let Err(e) = fs::remove_dir_all(&inst.dir)
        {
            tracing::debug!(
                "Failed to remove instance dir {} (non-fatal): {e}",
                inst.dir.display()
            );
        }
        Ok(())
    }

    fn destroy_shared(&self, cfg: &CoopConfig) {
        let images_dir = cfg.images_dir();
        if images_dir.exists()
            && let Err(e) = fs::remove_dir_all(&images_dir)
        {
            tracing::debug!("Failed to remove images dir (non-fatal): {e}");
        }
    }

    fn destroy_image(&self, cfg: &CoopConfig, image: &str) -> Result<()> {
        let dir = cfg.image_dir(image);
        if !dir.exists() {
            bail!("Image '{image}' does not exist");
        }
        fs::remove_dir_all(&dir)
            .with_context(|| format!("Failed to remove image dir {}", dir.display()))?;
        tracing::info!("Removed image '{image}'");
        Ok(())
    }

    fn resize_disk(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        new_size: crate::config::GiB,
    ) -> Result<()> {
        if self.is_running(inst) {
            bail!(
                "Instance '{}' is running — stop it first with \
                 `coop stop {}`",
                inst.name,
                inst.name,
            );
        }
        crate::lima::resize_disk(cfg, inst, new_size)
    }

    fn is_running(&self, inst: &Instance) -> bool {
        crate::lima::is_running(inst)
    }

    fn status(&self, cfg: &CoopConfig, inst: &Instance) -> Result<String> {
        crate::lima::status(cfg, inst)
    }

    fn stream_logs(&self, _cfg: &CoopConfig, inst: &Instance, follow: bool) -> Result<()> {
        crate::lima::stream_logs(inst, follow)
    }

    fn ssh_target(&self, cfg: &CoopConfig, inst: &Instance) -> Result<SshTarget> {
        crate::lima::ssh_target(cfg, inst)
    }

    fn disk_path(&self, inst: &Instance) -> Result<PathBuf> {
        crate::lima::disk_path(inst)
    }

    fn mounts_are_live(&self) -> bool {
        true
    }
}

// ── Platform type alias ───────────────────────────────────────

#[cfg(target_os = "macos")]
pub type PlatformBackend = LimaBackend;

#[cfg(not(target_os = "macos"))]
pub type PlatformBackend = FirecrackerBackend;

// ── Shared guest operations ───────────────────────────────────

/// Resolve tokens and build env vars to forward via SSH `SendEnv`.
///
/// Collects values from config and the process environment into an
/// `EnvForward` struct. No process-global env mutation.
///
/// `claude.api_key` values that use the `cmd:` prefix are resolved
/// here (at VM start time, not config parse time) so that secret
/// manager calls only happen when actually needed.
pub fn prepare_env_forwarding(cfg: &CoopConfig) -> Result<EnvForward> {
    let claude = &cfg.claude;
    let codex = &cfg.codex;
    let mut env = EnvForward::default();

    // ANTHROPIC_API_KEY: prefer config, fall back to process env
    if let Some(key) = &claude.api_key {
        let resolved =
            crate::config::resolve_cmd_value(key).context("Failed to resolve claude.api_key")?;
        env.set("ANTHROPIC_API_KEY", resolved);
    } else if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        env.set("ANTHROPIC_API_KEY", key);
    }

    // OPENAI_API_KEY: prefer config, fall back to process env
    if let Some(key) = &codex.api_key {
        let resolved =
            crate::config::resolve_cmd_value(key).context("Failed to resolve codex.api_key")?;
        env.set("OPENAI_API_KEY", resolved);
    } else if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        env.set("OPENAI_API_KEY", key);
    }

    // GITHUB_TOKEN: resolve via configured strategy
    if let Some(token) = resolve_github_token(cfg.github.as_ref()) {
        env.set("GITHUB_TOKEN", token);
    }

    // User-specified env_forward vars from process environment
    for name in &claude.env_forward {
        if !env.contains(name)
            && let Ok(val) = std::env::var(name)
        {
            env.set(name.as_str(), val);
        }
    }
    for name in &codex.env_forward {
        if !env.contains(name)
            && let Ok(val) = std::env::var(name)
        {
            env.set(name.as_str(), val);
        }
    }

    Ok(env)
}

/// Bootstrap configured guest agents in the guest declaratively.
pub fn bootstrap_agents(
    session: &SshSession<'_>,
    cfg: &CoopConfig,
    inst: &crate::config::Instance,
    restart: bool,
) -> Result<()> {
    // GitHub auth is guest-global state. Refresh it once before either
    // agent bootstrap if a token is available.
    if session.env.contains("GITHUB_TOKEN") {
        tracing::info!("Configuring GitHub auth in guest");
        setup_github_auth(session)?;
    }

    bootstrap_claude(session, cfg, inst, restart)?;
    bootstrap_codex(session, cfg, restart)?;

    Ok(())
}

/// Bootstrap Claude Code in the guest declaratively.
///
/// Runs the bootstrap sequence: GitHub auth, user content
/// (CLAUDE.md, rules), marketplaces, plugins, MCP servers.
///
/// On restart (`restart=true`), only refreshes ephemeral state
/// (GitHub auth, CLAUDE.md, rules). Marketplaces, plugins, and
/// MCP servers persist on the guest disk across stop/start.
///
/// Claude auth is NOT handled here — the user authenticates
/// when they first run `claude` interactively in the guest.
/// `ANTHROPIC_API_KEY` (if set) is forwarded via `SendEnv`
/// on every SSH session automatically.
fn bootstrap_claude(
    session: &SshSession<'_>,
    cfg: &CoopConfig,
    inst: &crate::config::Instance,
    restart: bool,
) -> Result<()> {
    let claude = &cfg.claude;

    if !restart {
        let needs_claude_cli = !claude.marketplaces.is_empty()
            || !claude.plugins.is_empty()
            || !claude.mcp_servers.is_empty();

        if needs_claude_cli
            && !session
                .target
                .exec_ok(&format!("test -x {}", crate::guest::CLAUDE_BIN))
        {
            bail!(
                "Claude Code CLI is not installed in the guest.\n\
                 The golden image may have been built before the \
                 installer was added, or the install failed silently.\n\
                 Run `coop setup --rebuild` to rebuild the image."
            );
        }
    }

    // User config (CLAUDE.md, rules/, commands/) — host files may have changed
    copy_claude_config(session.target, &claude.config_dir)?;

    // Marketplaces, plugins, MCP servers — persisted on guest disk,
    // only install on first boot
    if !restart {
        // Compute delta: only install marketplaces/plugins not already
        // baked into the golden image
        let (missing_marketplaces, missing_plugins) = compute_plugin_delta(cfg, &inst.image);

        if !missing_marketplaces.is_empty() {
            install_marketplaces(session, &missing_marketplaces)?;
        }

        if !missing_plugins.is_empty() {
            install_plugins(session, &missing_plugins)?;
        }

        if !claude.mcp_servers.is_empty() {
            register_mcp_servers(session, &claude.mcp_servers)?;
        }
    }

    tracing::info!("Claude Code bootstrap complete");
    Ok(())
}

/// Bootstrap Codex in the guest declaratively.
///
/// Codex uses `~/.codex/config.toml` for MCP registration and related
/// settings, so bootstrap writes allowlisted user config files and a
/// managed MCP section there when configured.
fn bootstrap_codex(session: &SshSession<'_>, cfg: &CoopConfig, restart: bool) -> Result<()> {
    let codex = &cfg.codex;
    let needs_codex = codex.config_dir != ConfigDir::Disabled || !codex.mcp_servers.is_empty();

    if !needs_codex {
        return Ok(());
    }

    if !session
        .target
        .exec_ok(&format!("test -x {}", crate::guest::CODEX_BIN))
    {
        bail!(
            "Codex CLI is not installed in the guest.\n\
             The golden image may have been built before Codex support \
             was added, or the install failed silently.\n\
             Run `coop setup --rebuild` to rebuild the image."
        );
    }

    copy_codex_config(session.target, codex)?;

    if restart {
        tracing::info!("Codex bootstrap refreshed");
    } else {
        tracing::info!("Codex bootstrap complete");
    }

    Ok(())
}

/// Compute which marketplaces and plugins are missing from the
/// golden image and need to be installed at start time.
fn compute_plugin_delta(cfg: &CoopConfig, image: &str) -> (Vec<String>, Vec<String>) {
    let baked = crate::setup::TemplateConfig::load_for(cfg, image).ok();

    let wanted_marketplaces = &cfg.claude.marketplaces;
    let wanted_plugins = &cfg.claude.plugins;

    match baked {
        Some(tc) => {
            let missing_m: Vec<String> = wanted_marketplaces
                .iter()
                .filter(|m| !tc.marketplaces.contains(m))
                .cloned()
                .collect();
            let missing_p: Vec<String> = wanted_plugins
                .iter()
                .filter(|p| !tc.plugins.contains(p))
                .cloned()
                .collect();
            (missing_m, missing_p)
        }
        None => (wanted_marketplaces.clone(), wanted_plugins.clone()),
    }
}

fn resolve_github_token(strategy: Option<&GitHubAuth>) -> Option<String> {
    // Default to Off — never forward tokens without explicit opt-in.
    // Users must set `github = "auto"` or `github = "env"` in
    // config.toml to enable GitHub auth in the guest.
    match strategy.unwrap_or(&GitHubAuth::Off) {
        GitHubAuth::Auto => std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(|| {
                Command::new("gh")
                    .args(["auth", "token"])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|t| !t.is_empty())
            }),
        GitHubAuth::Env => {
            let token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
            if token.is_none() {
                tracing::warn!(
                    "github: \"env\" requires GITHUB_TOKEN to be set. \
                     Private repo access will fail."
                );
            }
            token
        }
        GitHubAuth::Off => None,
    }
}

fn setup_github_auth(session: &SshSession<'_>) -> Result<()> {
    session
        .exec("gh auth setup-git")
        .context("Failed to configure git credential helper in guest")
}

/// Stage allowlisted files from a host Claude config directory into
/// a temp dir, then scp them to the guest's `~/.claude/`.
///
/// Allowlist: `CLAUDE.md`, `rules/`, `commands/`.
/// Missing source directory or missing individual entries are silently
/// skipped (debug-logged).
fn copy_claude_config(target: &SshTarget, config_dir: &ConfigDir) -> Result<()> {
    let Some(source_dir) = resolve_config_source_dir(config_dir, ".claude", "claude.config_dir")?
    else {
        return Ok(());
    };

    let staged = stage_selected_files(&source_dir, &["CLAUDE.md"], &["rules", "commands"])
        .context("Failed to stage Claude config files")?;

    let staging_path = staged.path();
    let has_entries = staging_path
        .read_dir()
        .context("Failed to read staging directory")?
        .next()
        .is_some();

    if !has_entries {
        tracing::debug!("No allowlisted files found in {}", source_dir.display());
        return Ok(());
    }

    target.exec("mkdir -p ~/.claude")?;

    let guest_claude = GuestPath::new("./.claude");
    for entry in std::fs::read_dir(staging_path).context("Failed to read staging directory")? {
        let entry = entry.context("Failed to read staging entry")?;
        let path = entry.path();
        if path.is_dir() {
            target
                .scp_to_recursive(&path, &guest_claude)
                .with_context(|| format!("Failed to copy {} to guest", path.display()))?;
        } else {
            target
                .scp_to(&path, &guest_claude)
                .with_context(|| format!("Failed to copy {} to guest", path.display()))?;
        }
    }

    tracing::info!("Copied Claude config from {}", source_dir.display());
    Ok(())
}

fn copy_codex_config(target: &SshTarget, codex: &crate::config::CodexConfig) -> Result<()> {
    let source_dir = resolve_config_source_dir(&codex.config_dir, ".codex", "codex.config_dir")?;
    let staged = stage_codex_files(source_dir.as_deref(), &codex.mcp_servers)
        .context("Failed to stage Codex config files")?;

    let staging_path = staged.path();
    let has_entries = staging_path
        .read_dir()
        .context("Failed to read staging directory")?
        .next()
        .is_some();

    if !has_entries {
        tracing::debug!("No Codex config content to copy");
        return Ok(());
    }

    target.exec("mkdir -p ~/.codex")?;

    let guest_codex = GuestPath::new("./.codex");
    for entry in std::fs::read_dir(staging_path).context("Failed to read staging directory")? {
        let entry = entry.context("Failed to read staging entry")?;
        let path = entry.path();
        if path.is_dir() {
            target
                .scp_to_recursive(&path, &guest_codex)
                .with_context(|| format!("Failed to copy {} to guest", path.display()))?;
        } else {
            target
                .scp_to(&path, &guest_codex)
                .with_context(|| format!("Failed to copy {} to guest", path.display()))?;
        }
    }

    tracing::info!("Copied Codex config into guest");
    Ok(())
}

fn resolve_config_source_dir(
    config_dir: &ConfigDir,
    default_dir_name: &str,
    label: &str,
) -> Result<Option<PathBuf>> {
    let path = match config_dir {
        ConfigDir::Disabled => {
            tracing::debug!("{label} is disabled, skipping");
            return Ok(None);
        }
        ConfigDir::Default => {
            let Some(home) = dirs::home_dir() else {
                tracing::debug!("Could not determine home directory, skipping config copy");
                return Ok(None);
            };
            home.join(default_dir_name)
        }
        ConfigDir::Custom(path) => path.clone(),
    };

    if !path.is_dir() {
        if matches!(config_dir, ConfigDir::Custom(_)) {
            tracing::warn!("{label} '{}' does not exist, skipping", path.display());
        } else {
            tracing::debug!(
                "Default config dir {} does not exist, skipping",
                path.display()
            );
        }
        return Ok(None);
    }

    Ok(Some(path))
}

/// Copy allowlisted entries from source into a temporary staging
/// directory. Returns the `TempDir` (caller keeps it alive).
fn stage_selected_files(
    source_dir: &Path,
    files: &[&str],
    dirs: &[&str],
) -> Result<tempfile::TempDir> {
    let staging = tempfile::TempDir::new().context("Failed to create staging directory")?;

    for file_name in files {
        let src = source_dir.join(file_name);
        if src.is_file() {
            std::fs::copy(&src, staging.path().join(file_name))
                .with_context(|| format!("Failed to stage {file_name}"))?;
            tracing::debug!("Staged {file_name}");
        }
    }

    for dir_name in dirs {
        let src = source_dir.join(dir_name);
        if src.is_dir() {
            copy_dir_recursive(&src, &staging.path().join(dir_name))
                .with_context(|| format!("Failed to stage {dir_name}/"))?;
            tracing::debug!("Staged {dir_name}/");
        }
    }

    Ok(staging)
}

fn stage_allowed_files(source_dir: &Path) -> Result<tempfile::TempDir> {
    stage_selected_files(source_dir, &["CLAUDE.md"], &["rules", "commands"])
}

fn stage_codex_files(
    source_dir: Option<&Path>,
    mcp_servers: &std::collections::HashMap<String, McpServerDef>,
) -> Result<tempfile::TempDir> {
    let staging = tempfile::TempDir::new().context("Failed to create staging directory")?;

    let mut config = match source_dir {
        Some(path) => {
            let staged = stage_selected_files(path, &["AGENTS.md"], &["prompts"])?;
            copy_dir_recursive(staged.path(), staging.path())
                .context("Failed to stage Codex allowlisted files")?;

            let config_path = path.join("config.toml");
            if config_path.is_file() {
                let content =
                    std::fs::read_to_string(&config_path).context("Failed to read config.toml")?;
                toml::from_str::<TomlValue>(&content)
                    .context("Failed to parse Codex config.toml")?
            } else {
                TomlValue::Table(Default::default())
            }
        }
        None => TomlValue::Table(Default::default()),
    };

    let resolved_servers = resolve_codex_mcp_servers(mcp_servers)?;
    if !resolved_servers.is_empty() {
        let TomlValue::Table(root) = &mut config else {
            bail!("Codex config.toml must deserialize to a TOML table");
        };
        root.insert(
            "mcp_servers".to_string(),
            TomlValue::try_from(resolved_servers)
                .context("Failed to serialize Codex MCP servers")?,
        );
    }

    let should_write_config = source_dir
        .map(|path| path.join("config.toml").is_file())
        .unwrap_or(false)
        || !mcp_servers.is_empty();

    if should_write_config {
        std::fs::write(
            staging.path().join("config.toml"),
            toml::to_string(&config).context("Failed to serialize Codex config.toml")?,
        )
        .context("Failed to stage Codex config.toml")?;
    }

    Ok(staging)
}

fn resolve_codex_mcp_servers(
    mcp_servers: &std::collections::HashMap<String, McpServerDef>,
) -> Result<std::collections::HashMap<String, McpServerDef>> {
    let mut resolved = std::collections::HashMap::with_capacity(mcp_servers.len());
    for (name, def) in mcp_servers {
        let mut cloned = def.clone();
        for (header_key, header_value) in &mut cloned.headers {
            *header_value = crate::config::resolve_cmd_value(header_value).with_context(|| {
                format!("Failed to resolve header '{header_key}' for Codex MCP server '{name}'")
            })?;
        }
        resolved.insert(name.clone(), cloned);
    }
    Ok(resolved)
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("Failed to create {}", dst.display()))?;
    for entry in
        std::fs::read_dir(src).with_context(|| format!("Failed to read {}", src.display()))?
    {
        let entry = entry.context("Failed to read directory entry")?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).with_context(|| {
                format!(
                    "Failed to copy {} -> {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        }
    }
    Ok(())
}

const GUEST_MARKETPLACE_DIR: &str = "~/.coop/marketplaces";

pub(crate) fn install_marketplaces(
    session: &SshSession<'_>,
    marketplaces: &[String],
) -> Result<()> {
    let mut has_local = false;

    for source in marketplaces {
        let local_path = Path::new(source);
        let guest_source = if local_path.is_absolute() && local_path.is_dir() {
            if !has_local {
                session
                    .target
                    .exec(&format!("mkdir -p {GUEST_MARKETPLACE_DIR}"))?;
                has_local = true;
            }
            let dir_name = local_path
                .file_name()
                .context("marketplace path has no directory name")?
                .to_string_lossy();
            let remote = GuestPath::new(format!("{GUEST_MARKETPLACE_DIR}/{dir_name}"));
            tracing::info!(
                "Copying local marketplace to guest: {} -> {remote}",
                local_path.display()
            );
            session
                .target
                .scp_to_recursive(local_path, &remote)
                .with_context(|| {
                    format!(
                        "Failed to copy marketplace '{}' to guest",
                        local_path.display()
                    )
                })?;
            remote.as_str().to_string()
        } else {
            source.clone()
        };

        tracing::info!("Adding marketplace: {guest_source}");
        let cmd = format!(
            "{} plugin marketplace add {} --scope user",
            crate::guest::CLAUDE_BIN,
            shell_escape(&guest_source),
        );
        session
            .exec(&cmd)
            .with_context(|| format!("Failed to add marketplace '{source}'"))?;
    }
    Ok(())
}

pub(crate) fn install_plugins(session: &SshSession<'_>, plugins: &[String]) -> Result<()> {
    for plugin in plugins {
        tracing::info!("Installing plugin: {plugin}");
        let cmd = format!(
            "{} plugin install {} -s user",
            crate::guest::CLAUDE_BIN,
            shell_escape(plugin),
        );
        session
            .exec(&cmd)
            .with_context(|| format!("Failed to install plugin '{plugin}'"))?;
    }
    Ok(())
}

fn register_mcp_servers(
    session: &SshSession<'_>,
    servers: &std::collections::HashMap<String, McpServerDef>,
) -> Result<()> {
    for (name, def) in servers {
        tracing::info!("Registering MCP server: {name}");

        // Resolve any `cmd:` prefixed header values before sending the
        // definition to the guest. Headers are the only secret-bearing
        // field in McpServerDef (`env` values are host env var names,
        // not secrets).
        let mut resolved = def.clone();
        for (header_key, header_value) in &mut resolved.headers {
            *header_value = crate::config::resolve_cmd_value(header_value).with_context(|| {
                format!("Failed to resolve header '{header_key}' for MCP server '{name}'")
            })?;
        }

        let json = serde_json::to_string(&resolved)
            .context("Failed to serialize MCP server definition")?;
        let cmd = format!(
            "{} mcp add-json -s user {} {}",
            crate::guest::CLAUDE_BIN,
            shell_escape(name),
            shell_escape(&json),
        );
        session
            .exec(&cmd)
            .with_context(|| format!("Failed to register MCP server '{name}'"))?;
    }
    Ok(())
}

/// Clone a git repository inside the guest VM via SSH.
pub fn clone_git_repo(target: &SshTarget, repo_url: &str) -> Result<()> {
    tracing::info!("Cloning {repo_url} into guest /workspace");

    let cmd = format!(
        "sudo mkdir -p /workspace && \
         sudo chown $(whoami):$(whoami) /workspace && \
         git clone {} /workspace/repo && \
         echo 'Repository cloned to /workspace/repo'",
        shell_escape(repo_url),
    );

    target
        .exec(&cmd)
        .context("Failed to clone git repo in guest")?;

    tracing::info!("Repository cloned to /workspace/repo");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    const SAMPLE_OUTPUT: &str = "\
0.12 0.08 0.03 1/42 1234
MemTotal:        2048000 kB
MemFree:          512000 kB
MemAvailable:    1024000 kB
Buffers:          128000 kB
Filesystem     1M-blocks  Used Available Use% Mounted on
/dev/vda1          20480  3200     16000  17% /
";

    #[test]
    fn parse_resource_usage_typical_output() {
        let usage = parse_resource_usage(SAMPLE_OUTPUT);
        assert!((usage.load_1m - 0.12).abs() < f64::EPSILON);
        assert_eq!(usage.mem_total_mib, 2000);
        assert_eq!(usage.mem_used_mib, 1000);
        assert_eq!(usage.disk_total_mib, 20480);
        assert_eq!(usage.disk_used_mib, 3200);
    }

    #[test]
    fn parse_resource_usage_empty_output() {
        let usage = parse_resource_usage("");
        assert!((usage.load_1m - 0.0).abs() < f64::EPSILON);
        assert_eq!(usage.mem_total_mib, 0);
        assert_eq!(usage.mem_used_mib, 0);
        assert_eq!(usage.disk_total_mib, 0);
        assert_eq!(usage.disk_used_mib, 0);
    }

    #[test]
    fn resource_usage_display() {
        let usage = ResourceUsage {
            load_1m: 1.5,
            mem_used_mib: 512,
            mem_total_mib: 2048,
            disk_used_mib: 5000,
            disk_total_mib: 20000,
        };
        let s = usage.to_string();
        assert!(s.contains("Load: 1.50"));
        assert!(s.contains("Mem: 512/2048 MiB (25%)"));
        assert!(s.contains("Disk: 5000/20000 MiB (25%)"));
    }

    #[test]
    fn resource_usage_summary() {
        let usage = ResourceUsage {
            load_1m: 0.42,
            mem_used_mib: 1024,
            mem_total_mib: 2048,
            disk_used_mib: 10000,
            disk_total_mib: 20000,
        };
        assert_eq!(usage.summary(), "load=0.42 mem=50% disk=50%");
    }

    #[test]
    fn parse_resource_usage_zero_mem_no_division_panic() {
        let output = "0.00 0.00 0.00 0/0 0\n";
        let usage = parse_resource_usage(output);
        assert_eq!(usage.mem_total_mib, 0);
        assert_eq!(usage.mem_used_mib, 0);
    }

    #[test]
    fn stage_allowed_files_copies_all_allowlisted() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("CLAUDE.md"), "# Config").unwrap();
        std::fs::create_dir(src.path().join("rules")).unwrap();
        std::fs::write(src.path().join("rules/a.md"), "rule a").unwrap();
        std::fs::create_dir(src.path().join("commands")).unwrap();
        std::fs::write(src.path().join("commands/b.md"), "cmd b").unwrap();

        let staging = stage_allowed_files(src.path()).unwrap();
        assert!(staging.path().join("CLAUDE.md").is_file());
        assert!(staging.path().join("rules/a.md").is_file());
        assert!(staging.path().join("commands/b.md").is_file());
        assert_eq!(
            std::fs::read_to_string(staging.path().join("CLAUDE.md")).unwrap(),
            "# Config"
        );
    }

    #[test]
    fn stage_allowed_files_skips_non_allowlisted() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("CLAUDE.md"), "ok").unwrap();
        std::fs::write(src.path().join("settings.json"), "secret").unwrap();
        std::fs::create_dir(src.path().join("projects")).unwrap();
        std::fs::write(src.path().join("projects/data"), "nope").unwrap();

        let staging = stage_allowed_files(src.path()).unwrap();
        assert!(staging.path().join("CLAUDE.md").is_file());
        assert!(!staging.path().join("settings.json").exists());
        assert!(!staging.path().join("projects").exists());
    }

    #[test]
    fn stage_allowed_files_empty_source() {
        let src = tempfile::TempDir::new().unwrap();
        let staging = stage_allowed_files(src.path()).unwrap();
        let count = std::fs::read_dir(staging.path()).unwrap().count();
        assert_eq!(count, 0);
    }

    #[test]
    fn stage_codex_files_merges_mcp_servers_into_config() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("AGENTS.md"), "Global instructions").unwrap();
        std::fs::write(src.path().join("config.toml"), "model = \"gpt-5\"\n").unwrap();

        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "sentry".to_string(),
            McpServerDef {
                command: None,
                args: Vec::new(),
                server_type: Some("http".to_string()),
                url: Some("https://mcp.sentry.dev/mcp".to_string()),
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let staging = stage_codex_files(Some(src.path()), &servers).unwrap();
        assert!(staging.path().join("AGENTS.md").is_file());

        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert!(config.contains("model = \"gpt-5\""));
        assert!(config.contains("[mcp_servers.sentry]"));
        assert!(config.contains("url = \"https://mcp.sentry.dev/mcp\""));
    }

    #[test]
    fn copy_dir_recursive_nested() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(src.path().join("a/b")).unwrap();
        std::fs::write(src.path().join("a/b/c.txt"), "nested").unwrap();

        let dst = tempfile::TempDir::new().unwrap();
        let target = dst.path().join("out");
        copy_dir_recursive(src.path().join("a").as_path(), &target).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("b/c.txt")).unwrap(),
            "nested"
        );
    }
}
