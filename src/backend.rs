#[cfg(target_os = "macos")]
use std::fs;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use toml::Value as TomlValue;

use crate::cmd::Cmd;
use crate::config::{ConfigDir, CoopConfig, GitHubAuth, ImageName, Instance, McpServerDef};
use crate::paths::{GuestPath, HostPath};
use crate::setup::SetupOptions;
use crate::shell::shell_escape;

// ── Operation modes ───────────────────────────────────────────

/// Whether an agent bootstrap is running for the first boot of a new
/// VM or for a restart of an existing one. On restart, marketplaces,
/// plugins, and MCP servers are skipped because they persist on the
/// guest disk across stop/start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMode {
    FirstBoot,
    Restart,
}

/// Whether log streaming reads the existing log once and exits or
/// tails it indefinitely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMode {
    Snapshot,
    Follow,
}

// ── Environment forwarding ────────────────────────────────────

/// Environment variables to forward to guest VMs via SSH `SendEnv`.
///
/// Carries both variable names (for `-o SendEnv=`) and their values
/// (for `Command::env()` on SSH child processes), avoiding unsafe
/// mutation of the process-global environment.
///
/// The whole struct is secret-bearing by construction (entries are
/// `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GITHUB_TOKEN`, plus any
/// user-configured `env_forward` values), so `Debug` redacts every
/// value. Variable *names* are preserved because they are useful in
/// diagnostics and are not themselves secret.
#[derive(Clone, Default)]
pub struct EnvForward {
    vars: IndexMap<String, String>,
}

impl std::fmt::Debug for EnvForward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut dbg = f.debug_struct("EnvForward");
        // `debug_map` would print keys without the `vars:` prefix; using
        // a struct field with a map matches what `derive(Debug)` produced
        // before, minus the redaction.
        dbg.field(
            "vars",
            &self
                .vars
                .keys()
                .map(|k| (k.as_str(), "<redacted>"))
                .collect::<IndexMap<&str, &str>>(),
        );
        dbg.finish()
    }
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

/// Proof that an instance is currently running. Construct via
/// [`VmBackend::as_running`] — the constructor is the single place
/// that probes live state, so operations taking a `RunningInstance`
/// can rely on the precondition without re-checking. Carries the
/// SSH target so connection details are always available without
/// further fallible lookups.
///
/// Fields are private so a `RunningInstance` cannot be forged; the
/// only way to obtain one is through a backend method that verified
/// the instance is alive.
pub struct RunningInstance {
    inst: Instance,
    target: SshTarget,
}

impl RunningInstance {
    /// Mint a `RunningInstance` after a successful live-state probe.
    ///
    /// Crate-private so only backend impls can construct one. Callers
    /// use [`VmBackend::as_running`] (which delegates here).
    pub(crate) fn new(inst: Instance, target: SshTarget) -> Self {
        Self { inst, target }
    }

    pub fn instance(&self) -> &Instance {
        &self.inst
    }

    pub fn target(&self) -> &SshTarget {
        &self.target
    }

    /// Consume the wrapper and return the inner `Instance` and SSH target.
    pub fn into_parts(self) -> (Instance, SshTarget) {
        (self.inst, self.target)
    }
}

// ── SSH target ────────────────────────────────────────────────

const MAX_HOSTNAME_LEN: usize = 253;

fn validate_hostname(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Hostname must not be empty");
    }
    if name.len() > MAX_HOSTNAME_LEN {
        bail!(
            "Hostname too long ({} chars, max {MAX_HOSTNAME_LEN})",
            name.len()
        );
    }
    if let Some(c) = name
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || !c.is_ascii_graphic())
    {
        bail!("Hostname contains invalid character {c:?} (must be printable ASCII, no whitespace)");
    }
    Ok(())
}

/// Validated SSH hostname or IP literal. Construction enforces a
/// non-empty, printable ASCII string with no whitespace, so downstream
/// code (shell-escaped SSH args, `user@host` formatting, SSH config
/// blocks) can use it without re-checking.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hostname(String);

impl Hostname {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_hostname(&name)?;
        Ok(Self(name))
    }
}

impl std::fmt::Display for Hostname {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Hostname {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn as_ref(&self) -> &str {
        &self.0
    }
}

const MAX_SSH_USER_LEN: usize = 32;

fn validate_ssh_user(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("SSH user must not be empty");
    }
    if name.len() > MAX_SSH_USER_LEN {
        bail!(
            "SSH user too long ({} chars, max {MAX_SSH_USER_LEN})",
            name.len()
        );
    }
    for (i, c) in name.chars().enumerate() {
        let ok = if i == 0 {
            matches!(c, 'a'..='z' | '_')
        } else {
            matches!(c, 'a'..='z' | '0'..='9' | '_' | '-')
        };
        if !ok {
            if i == 0 {
                bail!("SSH user must start with [a-z_], got {c:?}");
            }
            bail!("SSH user contains invalid character {c:?} (allowed: a-z, 0-9, '_', '-')");
        }
    }
    Ok(())
}

/// Validated SSH username. Construction enforces the portable POSIX
/// pattern `[a-z_][a-z0-9_-]{0,31}`, so downstream code (SSH addr
/// formatting, SSH config blocks) can use it without re-checking.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SshUser(String);

impl SshUser {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_ssh_user(&name)?;
        Ok(Self(name))
    }
}

impl std::fmt::Display for SshUser {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SshUser {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// SSH connection details for reaching a guest VM.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: Hostname,
    pub port: NonZeroU16,
    pub user: SshUser,
    pub key_path: PathBuf,
}

// ── SSH session ───────────────────────────────────────────────

/// SSH operations that combine connection details with environment
/// forwarding. Owns both so call sites can construct a session once
/// and pass it around without juggling lifetimes; `SshTarget` and
/// `EnvForward` are cheap to clone (small string buffers).
pub struct SshSession {
    pub target: SshTarget,
    pub env: EnvForward,
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
            "IdentitiesOnly=yes".into(),
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
            "IdentitiesOnly=yes".into(),
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

    /// Run a command on the guest via SSH, piping `stdin` to the remote shell.
    ///
    /// The bytes are written to ssh's stdin and forwarded to the remote shell.
    /// Use this when the command needs to consume secrets that must not appear
    /// on argv or in the SSH debug log — e.g. tokens read via `read -r VAR`.
    pub fn exec_with_stdin(&self, cmd: &str, stdin: Vec<u8>) -> Result<()> {
        let mut args = self.ssh_opts();
        args.push(self.addr());
        args.push(cmd.to_string());
        Cmd::new("ssh")
            .args(args)
            .stdin_input(stdin)
            .run()
            .with_context(|| format!("SSH command failed: {cmd}"))
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
    pub fn scp_to(&self, local: &HostPath, remote: &GuestPath) -> Result<()> {
        let status = Command::new("scp")
            .args(self.scp_opts())
            .arg(local.as_path())
            .arg(format!("{}:{remote}", self.addr()))
            .status()
            .context("Failed to run scp")?;

        if !status.success() {
            bail!("scp failed: {} -> {remote}", local.as_path().display());
        }
        Ok(())
    }

    /// Copy a local directory to the guest recursively via scp.
    pub fn scp_to_recursive(&self, local: &HostPath, remote: &GuestPath) -> Result<()> {
        let status = Command::new("scp")
            .args(self.scp_opts())
            .arg("-r")
            .arg(local.as_path())
            .arg(format!("{}:{remote}", self.addr()))
            .status()
            .context("Failed to run scp")?;

        if !status.success() {
            bail!("scp -r failed: {} -> {remote}", local.as_path().display());
        }
        Ok(())
    }

    /// SSH command string for rsync's -e flag.
    pub fn rsync_ssh_cmd(&self) -> String {
        format!(
            "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
             -o IdentitiesOnly=yes -o LogLevel=ERROR -i {} -p {}",
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

impl SshSession {
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
    /// Stop a running instance. Consumes the `RunningInstance` proof
    /// so the type system witnesses that the precondition held when
    /// the call was made.
    fn stop(&self, cfg: &CoopConfig, running: RunningInstance) -> Result<()>;
    fn destroy_instance(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()>;
    fn destroy_shared(&self, cfg: &CoopConfig);
    fn destroy_image(&self, cfg: &CoopConfig, image: &ImageName) -> Result<()>;
    fn resize_disk(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        new_size: crate::config::GiB,
    ) -> Result<()>;
    fn is_running(&self, inst: &Instance) -> bool;
    /// Probe the live state of `inst` and return a `RunningInstance`
    /// if it is running. This is the single chokepoint for "is this
    /// VM alive?" — call sites that need to operate on a running VM
    /// should ask via this method rather than open-coding the check.
    ///
    /// Returns `Err` when the instance is not running or when the
    /// backend lookup itself fails (e.g. `limactl list` errors). The
    /// `Instance` is consumed; on `Err` callers can clone before
    /// calling if they need to keep working with it.
    fn as_running(&self, cfg: &CoopConfig, inst: Instance) -> Result<RunningInstance>;
    fn status(&self, cfg: &CoopConfig, inst: &Instance) -> Result<String>;
    /// Stream logs from a running instance.
    ///
    /// Takes `&RunningInstance` so the running precondition is part
    /// of the signature — Firecracker's log streaming asserts the
    /// PID is alive, and `--follow` only makes sense while the VM
    /// is producing new output.
    fn stream_logs(&self, cfg: &CoopConfig, running: &RunningInstance, mode: LogMode)
    -> Result<()>;
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

    fn stop(&self, cfg: &CoopConfig, running: RunningInstance) -> Result<()> {
        let (inst, _target) = running.into_parts();
        let vm = crate::vm::FirecrackerVm::from_running(cfg, &inst)?;
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

    fn destroy_image(&self, cfg: &CoopConfig, image: &ImageName) -> Result<()> {
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

    fn as_running(&self, cfg: &CoopConfig, inst: Instance) -> Result<RunningInstance> {
        if !inst.is_running() {
            bail!(
                "Instance '{}' is not running (no live Firecracker process \
                 for PID file {})",
                inst.name,
                inst.pid_file_path().display(),
            );
        }
        let target = self.ssh_target(cfg, &inst)?;
        Ok(RunningInstance::new(inst, target))
    }

    fn status(&self, cfg: &CoopConfig, inst: &Instance) -> Result<String> {
        let vm = crate::vm::FirecrackerVm::from_running(cfg, inst)?;
        vm.status()
    }

    fn stream_logs(
        &self,
        cfg: &CoopConfig,
        running: &RunningInstance,
        mode: LogMode,
    ) -> Result<()> {
        let vm = crate::vm::FirecrackerVm::from_running(cfg, running.instance())?;
        vm.stream_logs(mode)
    }

    fn ssh_target(&self, cfg: &CoopConfig, inst: &Instance) -> Result<SshTarget> {
        Ok(SshTarget {
            host: Hostname::new(inst.guest_ip())?,
            port: cfg.ssh_port,
            user: SshUser::new(crate::guest::GUEST_USER)?,
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

    fn stop(&self, _cfg: &CoopConfig, running: RunningInstance) -> Result<()> {
        let (inst, _target) = running.into_parts();
        crate::lima::stop_running(&inst)
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

    fn destroy_image(&self, cfg: &CoopConfig, image: &ImageName) -> Result<()> {
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

    fn as_running(&self, cfg: &CoopConfig, inst: Instance) -> Result<RunningInstance> {
        if !crate::lima::is_running(&inst) {
            bail!(
                "Instance '{}' is not running (Lima reports state != Running)",
                inst.name,
            );
        }
        let target = crate::lima::ssh_target(cfg, &inst)?;
        Ok(RunningInstance::new(inst, target))
    }

    fn status(&self, cfg: &CoopConfig, inst: &Instance) -> Result<String> {
        crate::lima::status(cfg, inst)
    }

    fn stream_logs(
        &self,
        _cfg: &CoopConfig,
        running: &RunningInstance,
        mode: LogMode,
    ) -> Result<()> {
        crate::lima::stream_logs(running.instance(), mode)
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

/// Detect the GitHub `owner/repo` slug for an existing instance, if any.
///
/// Looks at the saved workspace state in priority order: a `git-repo`
/// clone records the original URL (parsed for a slug); a `workspace` /
/// `mount` source has a host path we can scan for a `.git/config`
/// `origin`. Returns `None` when no slug can be derived — pat-mode
/// then falls back to a clear error.
pub fn detect_instance_repo(
    inst: &crate::config::Instance,
) -> Option<crate::github_repo::RepoSlug> {
    use crate::workspace::WorkspaceSource;

    let state = crate::workspace::WorkspaceState::try_load(inst)
        .ok()
        .flatten()?;
    match &state.source {
        WorkspaceSource::GitRepo { url } => crate::github_repo::parse_repo_slug_from_url(url),
        WorkspaceSource::Workspace { host_path } | WorkspaceSource::Mount { host_path } => {
            crate::github_repo::detect_workspace_repo(host_path)
                .ok()
                .flatten()
        }
    }
}

/// Resolve tokens and build env vars to forward via SSH `SendEnv`.
///
/// Collects values from config and the process environment into an
/// `EnvForward` struct. No process-global env mutation.
///
/// `claude.api_key` values that use the `cmd:` prefix are resolved
/// here (at VM start time, not config parse time) so that secret
/// manager calls only happen when actually needed.
///
/// When `github = "pat"`, `repo` selects the matching entry under
/// `[github.pat]`. Pass the slug resolved from `--git-repo` or
/// `git remote get-url origin` of the workspace. Pass `None` when no
/// repo context is available; pat-mode then yields no token (the start
/// flow surfaces a clearer error elsewhere).
pub fn prepare_env_forwarding(
    cfg: &CoopConfig,
    repo: Option<&crate::github_repo::RepoSlug>,
) -> Result<EnvForward> {
    let claude = &cfg.claude;
    let codex = &cfg.codex;
    let mut env = EnvForward::default();

    // ANTHROPIC_API_KEY: prefer config, fall back to process env
    if let Some(key) = &claude.api_key {
        let resolved = crate::config::resolve_cmd_value(key.expose())
            .context("Failed to resolve claude.api_key")?;
        env.set("ANTHROPIC_API_KEY", resolved);
    } else if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        env.set("ANTHROPIC_API_KEY", key);
    }

    // OPENAI_API_KEY: prefer config, fall back to process env
    if let Some(key) = &codex.api_key {
        let resolved = crate::config::resolve_cmd_value(key.expose())
            .context("Failed to resolve codex.api_key")?;
        env.set("OPENAI_API_KEY", resolved);
    } else if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        env.set("OPENAI_API_KEY", key);
    }

    // GITHUB_TOKEN: resolve via configured strategy
    if let Some(token) = resolve_github_token(cfg.github.as_ref(), repo)? {
        env.set("GITHUB_TOKEN", token);
    } else {
        tracing::debug!("no GITHUB_TOKEN forwarded to guest");
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

    // `guest_env` literals override anything resolved above (forwarded
    // host env vars, `claude.api_key`, etc.) so an explicit value beats
    // an inherited one. Warn on collision so the override is visible —
    // values are not logged (they may be secrets).
    for (name, value) in &cfg.guest_env {
        if env.contains(name.as_str()) {
            tracing::warn!("guest_env entry '{name}' overrides a previously resolved value");
        }
        env.set(name.as_str(), value.as_str());
    }

    Ok(env)
}

/// Run a user-supplied post-start hook in the guest.
///
/// The command is sent to SSH as-is and evaluated by the guest's login
/// shell, so pipes, `&&`, and redirects all work. Failures are logged at
/// `WARN` and swallowed — a transient hook failure shouldn't strand the VM.
pub fn run_post_start(session: &SshSession, command: &str) {
    tracing::info!("Running post_start hook in guest");
    tracing::debug!("post_start: {command}");

    match session.exec(command) {
        Ok(()) => tracing::debug!("post_start hook completed"),
        Err(e) => tracing::warn!("post_start hook failed (continuing): {e}"),
    }
}

/// Bootstrap configured guest agents in the guest declaratively.
pub fn bootstrap_agents(
    session: &SshSession,
    cfg: &CoopConfig,
    inst: &crate::config::Instance,
    mode: BootMode,
) -> Result<()> {
    // GitHub auth is guest-global state. Refresh it once before either
    // agent bootstrap if a token is available.
    if session.env.contains("GITHUB_TOKEN") {
        tracing::info!("Configuring GitHub auth in guest");
        setup_github_auth(session)?;
    }

    bootstrap_claude(session, cfg, inst, mode)?;
    bootstrap_codex(session, cfg, mode)?;

    Ok(())
}

/// Bootstrap Claude Code in the guest declaratively.
///
/// Runs the bootstrap sequence: GitHub auth, user content
/// (CLAUDE.md, rules), marketplaces, plugins, MCP servers.
///
/// On `BootMode::Restart`, only refreshes ephemeral state
/// (GitHub auth, CLAUDE.md, rules). Marketplaces, plugins, and
/// MCP servers persist on the guest disk across stop/start.
///
/// Claude auth is NOT handled here — the user authenticates
/// when they first run `claude` interactively in the guest.
/// `ANTHROPIC_API_KEY` (if set) is forwarded via `SendEnv`
/// on every SSH session automatically.
fn bootstrap_claude(
    session: &SshSession,
    cfg: &CoopConfig,
    inst: &crate::config::Instance,
    mode: BootMode,
) -> Result<()> {
    let claude = &cfg.claude;

    if let BootMode::FirstBoot = mode {
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
    copy_claude_config(&session.target, &claude.config_dir)?;

    // Managed permissions: pre-accept bypass mode so `coop ca` and
    // `coop claude` skip prompts without an interactive acceptance step.
    write_managed_claude_settings(&session.target)?;

    // Marketplaces, plugins, MCP servers — persisted on guest disk,
    // only install on first boot
    if let BootMode::FirstBoot = mode {
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
fn bootstrap_codex(session: &SshSession, cfg: &CoopConfig, mode: BootMode) -> Result<()> {
    let codex = &cfg.codex;
    let source_dir = resolve_config_source_dir(&codex.config_dir, ".codex", "codex.config_dir");
    let needs_codex = codex_bootstrap_needed(source_dir.as_deref(), &codex.mcp_servers);

    if !needs_codex {
        return Ok(());
    }

    if !session
        .target
        .exec_ok(&format!("test -x {}", crate::guest::CODEX_BIN))
    {
        bail!("{}", codex_missing_guest_cli_message());
    }

    copy_codex_config(&session.target, source_dir.as_deref(), codex)?;

    match mode {
        BootMode::Restart => tracing::info!("Codex bootstrap refreshed"),
        BootMode::FirstBoot => tracing::info!("Codex bootstrap complete"),
    }

    Ok(())
}

fn codex_missing_guest_cli_message() -> &'static str {
    "Codex CLI is not installed in the guest.\n\
     The golden image may have been built before Codex support \
     was added, or the install failed silently.\n\
     If you want to skip Codex bootstrap for now, retry with \
     `--no-agents` (the `--no-claude` alias is deprecated).\n\
     Otherwise run `coop setup --rebuild` to rebuild the image."
}

/// Compute which marketplaces and plugins are missing from the
/// golden image and need to be installed at start time.
fn compute_plugin_delta(cfg: &CoopConfig, image: &ImageName) -> (Vec<String>, Vec<String>) {
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

/// Resolve a GitHub token for the guest given the configured auth strategy
/// and the resolved target repo (when known).
///
/// Returns `Ok(None)` when no token should be forwarded; errors when a
/// strategy was selected but the lookup is unrecoverable (e.g. pat-mode
/// with an entry that fails to resolve).
fn resolve_github_token(
    strategy: Option<&GitHubAuth>,
    repo: Option<&crate::github_repo::RepoSlug>,
) -> Result<Option<String>> {
    // Default to Off — never forward tokens without explicit opt-in.
    // Users must set `github = "auto"` / `"env"` / `"pat"` in
    // config.toml to enable GitHub auth in the guest.
    match strategy.unwrap_or(&GitHubAuth::Off) {
        GitHubAuth::Auto => Ok(std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(gh_auth_token)),
        GitHubAuth::Env => {
            let token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
            if token.is_none() {
                tracing::warn!(
                    "github: \"env\" requires GITHUB_TOKEN to be set. \
                     Private repo access will fail."
                );
            }
            Ok(token)
        }
        GitHubAuth::Off => Ok(None),
        GitHubAuth::Pat(_) => {
            let Some(slug) = repo else {
                tracing::warn!(
                    "github: \"pat\" requires a resolvable repo (via --git-repo or \
                     workspace origin). No token will be forwarded."
                );
                return Ok(None);
            };
            // Missing entry is not fatal: follow-up commands (shell / exec /
            // claude / codex / restart) must still work without a token,
            // matching the "off" mode shape. The wizard's pre-flight prompt
            // is the discovery path for missing entries.
            if strategy.and_then(|s| s.pat_entry(slug)).is_none() {
                tracing::warn!(
                    "github: \"pat\" mode has no [github.pat.\"{slug}\"] entry. \
                     No token forwarded. Run `coop github setup-pat --repo {slug}` \
                     to add one."
                );
                return Ok(None);
            }
            resolve_pat_token(strategy, slug).map(Some)
        }
    }
}

/// Look up a `[github.pat."repo"]` entry and resolve its `token` via
/// the `cmd:` indirection.
fn resolve_pat_token(
    strategy: Option<&GitHubAuth>,
    repo: &crate::github_repo::RepoSlug,
) -> Result<String> {
    let entry = strategy
        .and_then(|s| s.pat_entry(repo))
        .with_context(|| crate::github_pat::missing_entry_error(repo))?;
    let token = crate::config::resolve_cmd_value(entry.token.expose())
        .with_context(|| format!("Failed to resolve token for [github.pat.\"{repo}\"]"))?;
    if !token.starts_with(crate::github_pat::TOKEN_PREFIX) {
        tracing::warn!(
            "github.pat.\"{repo}\".token did not start with '{prefix}' — proceeding, \
             but the FGPAT server-side scope guarantees do not apply.",
            prefix = crate::github_pat::TOKEN_PREFIX,
        );
    }
    Ok(token)
}

fn setup_github_auth(session: &SshSession) -> Result<()> {
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
    let Some(source_dir) = resolve_config_source_dir(config_dir, ".claude", "claude.config_dir")
    else {
        return Ok(());
    };

    let staged = stage_allowed_files(&source_dir).context("Failed to stage Claude config files")?;

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
        let local = HostPath::new(&path);
        if path.is_dir() {
            target
                .scp_to_recursive(&local, &guest_claude)
                .with_context(|| format!("Failed to copy {} to guest", path.display()))?;
        } else {
            target
                .scp_to(&local, &guest_claude)
                .with_context(|| format!("Failed to copy {} to guest", path.display()))?;
        }
    }

    tracing::info!("Copied Claude config from {}", source_dir.display());
    Ok(())
}

/// JSON body of the managed `~/.claude/settings.json` written to every guest.
///
/// `skipDangerousModePermissionPrompt: true` pre-accepts bypass mode so that
/// `claude agents` and `claude --bg` honor `--permission-mode
/// bypassPermissions`. Without it, those commands refuse the mode until
/// it has been accepted interactively — a step that never happens on a
/// freshly provisioned VM.
///
/// `defaultMode: "bypassPermissions"` makes bypass the default for every
/// session in the VM, matching coop's design: the VM is the isolation
/// boundary, so per-session permission prompts add no protection.
///
/// The setting must live in user scope (`~/.claude/settings.json`) — Claude
/// Code ignores `skipDangerousModePermissionPrompt` from project settings
/// to prevent untrusted repositories from auto-bypassing the prompt.
fn managed_claude_settings_json() -> String {
    serde_json::json!({
        "permissions": {
            "defaultMode": "bypassPermissions",
            "skipDangerousModePermissionPrompt": true,
        }
    })
    .to_string()
}

/// Write coop's managed `~/.claude/settings.json` to the guest.
///
/// Runs every `coop start`, overwriting any in-guest edits. The file
/// is small and owned by coop; users wanting per-VM customization should
/// extend coop's config rather than editing the guest file in place.
fn write_managed_claude_settings(target: &SshTarget) -> Result<()> {
    target.exec("mkdir -p ~/.claude")?;
    target
        .exec_with_stdin(
            "cat > ~/.claude/settings.json",
            managed_claude_settings_json().into_bytes(),
        )
        .context("Failed to write managed ~/.claude/settings.json in guest")?;
    tracing::debug!("Wrote managed ~/.claude/settings.json to guest");
    Ok(())
}

fn copy_codex_config(
    target: &SshTarget,
    source_dir: Option<&Path>,
    codex: &crate::config::CodexConfig,
) -> Result<()> {
    let staged = stage_codex_files(source_dir, &codex.mcp_servers)
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
        let local = HostPath::new(&path);
        if path.is_dir() {
            target
                .scp_to_recursive(&local, &guest_codex)
                .with_context(|| format!("Failed to copy {} to guest", path.display()))?;
        } else {
            target
                .scp_to(&local, &guest_codex)
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
) -> Option<PathBuf> {
    let path = match config_dir {
        ConfigDir::Disabled => {
            tracing::debug!("{label} is disabled, skipping");
            return None;
        }
        ConfigDir::Default => {
            let Some(home) = dirs::home_dir() else {
                tracing::debug!("Could not determine home directory, skipping config copy");
                return None;
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
        return None;
    }

    Some(path)
}

/// Copy allowlisted entries from source into the target staging directory.
fn stage_selected_files_into(
    source_dir: &Path,
    staging_dir: &Path,
    files: &[&str],
    dirs: &[&str],
) -> Result<()> {
    for file_name in files {
        let src = source_dir.join(file_name);
        if src.is_file() {
            std::fs::copy(&src, staging_dir.join(file_name))
                .with_context(|| format!("Failed to stage {file_name}"))?;
            tracing::debug!("Staged {file_name}");
        }
    }

    for dir_name in dirs {
        let src = source_dir.join(dir_name);
        if src.is_dir() {
            copy_dir_recursive(&src, &staging_dir.join(dir_name))
                .with_context(|| format!("Failed to stage {dir_name}/"))?;
            tracing::debug!("Staged {dir_name}/");
        }
    }

    Ok(())
}

/// Copy allowlisted entries from source into a temporary staging
/// directory. Returns the `TempDir` (caller keeps it alive).
fn stage_selected_files(
    source_dir: &Path,
    files: &[&str],
    dirs: &[&str],
) -> Result<tempfile::TempDir> {
    let staging = tempfile::TempDir::new().context("Failed to create staging directory")?;
    stage_selected_files_into(source_dir, staging.path(), files, dirs)?;
    Ok(staging)
}

fn stage_allowed_files(source_dir: &Path) -> Result<tempfile::TempDir> {
    stage_selected_files(source_dir, &["CLAUDE.md"], &["rules", "commands"])
}

/// Allowlisted files copied verbatim from the host Codex config dir.
const CODEX_ALLOWED_FILES: &[&str] = &["AGENTS.md", "auth.json"];

/// Allowlisted directories copied recursively from the host Codex config dir.
const CODEX_ALLOWED_DIRS: &[&str] = &["prompts"];

/// Codex config file merged with managed MCP servers rather than copied verbatim.
const CODEX_CONFIG_FILE: &str = "config.toml";

fn stage_codex_files(
    source_dir: Option<&Path>,
    mcp_servers: &std::collections::HashMap<String, McpServerDef>,
) -> Result<tempfile::TempDir> {
    let staging = tempfile::TempDir::new().context("Failed to create staging directory")?;

    let mut config = match source_dir {
        Some(path) => {
            stage_selected_files_into(
                path,
                staging.path(),
                CODEX_ALLOWED_FILES,
                CODEX_ALLOWED_DIRS,
            )
            .context("Failed to stage Codex allowlisted files")?;

            let config_path = path.join(CODEX_CONFIG_FILE);
            if config_path.is_file() {
                let content = std::fs::read_to_string(&config_path)
                    .with_context(|| format!("Failed to read {CODEX_CONFIG_FILE}"))?;
                toml::from_str::<TomlValue>(&content)
                    .with_context(|| format!("Failed to parse Codex {CODEX_CONFIG_FILE}"))?
            } else {
                TomlValue::Table(toml::map::Map::default())
            }
        }
        None => TomlValue::Table(toml::map::Map::default()),
    };

    let resolved_servers = resolve_codex_mcp_servers(mcp_servers)?;
    if !resolved_servers.is_empty() {
        let TomlValue::Table(root) = &mut config else {
            bail!("Codex {CODEX_CONFIG_FILE} must deserialize to a TOML table");
        };
        if root.contains_key("mcp_servers") {
            tracing::warn!(
                "Replacing existing [mcp_servers] in Codex {CODEX_CONFIG_FILE} with servers from coop config"
            );
        }
        root.insert(
            "mcp_servers".to_string(),
            TomlValue::try_from(resolved_servers)
                .context("Failed to serialize Codex MCP servers")?,
        );
    }

    let should_write_config = source_dir.is_some_and(|path| path.join(CODEX_CONFIG_FILE).is_file())
        || !mcp_servers.is_empty();

    if should_write_config {
        std::fs::write(
            staging.path().join(CODEX_CONFIG_FILE),
            toml::to_string(&config)
                .with_context(|| format!("Failed to serialize Codex {CODEX_CONFIG_FILE}"))?,
        )
        .with_context(|| format!("Failed to stage Codex {CODEX_CONFIG_FILE}"))?;
    }

    Ok(staging)
}

fn codex_source_has_bootstrap_content(source_dir: Option<&Path>) -> bool {
    source_dir.is_some_and(|path| {
        CODEX_ALLOWED_FILES.iter().any(|f| path.join(f).is_file())
            || path.join(CODEX_CONFIG_FILE).is_file()
            || CODEX_ALLOWED_DIRS.iter().any(|d| path.join(d).is_dir())
    })
}

fn codex_bootstrap_needed(
    source_dir: Option<&Path>,
    mcp_servers: &std::collections::HashMap<String, McpServerDef>,
) -> bool {
    codex_source_has_bootstrap_content(source_dir) || !mcp_servers.is_empty()
}

fn resolve_codex_mcp_servers(
    mcp_servers: &std::collections::HashMap<String, McpServerDef>,
) -> Result<std::collections::HashMap<String, McpServerDef>> {
    let mut resolved = std::collections::HashMap::with_capacity(mcp_servers.len());
    for (name, def) in mcp_servers {
        let mut cloned = def.clone();
        cloned.resolve_header_secrets("Codex MCP server", name)?;
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

pub(crate) fn install_marketplaces(session: &SshSession, marketplaces: &[String]) -> Result<()> {
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
                .scp_to_recursive(&HostPath::from(local_path), &remote)
                .with_context(|| {
                    format!(
                        "Failed to copy marketplace '{}' to guest",
                        local_path.display()
                    )
                })?;
            remote.to_string()
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

pub(crate) fn install_plugins(session: &SshSession, plugins: &[String]) -> Result<()> {
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
    session: &SshSession,
    servers: &std::collections::HashMap<String, McpServerDef>,
) -> Result<()> {
    for (name, def) in servers {
        tracing::info!("Registering MCP server: {name}");

        let mut resolved = def.clone();
        resolved.resolve_header_secrets("MCP server", name)?;

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
///
/// For GitHub HTTPS URLs, resolves a token on the host and forwards it to
/// git in the guest via stdin and a one-shot credential helper. Token
/// resolution honours the configured GitHub strategy:
///
/// - `github = "pat"` with a matching `[github.pat."owner/repo"]` entry
///   uses the configured PAT. This is the user's explicit per-repo intent,
///   so it takes precedence over the host-side fallback. If the entry's
///   `cmd:` resolution fails, the error is propagated rather than silently
///   substituting a broader-scoped host token.
/// - Every other configuration (`off`, `auto`, `env`, or `pat` with no
///   matching entry) opportunistically uses `gh auth token` then
///   `GITHUB_TOKEN` — see [`host_github_token`] for why the non-PAT modes
///   don't gate this fallback.
///
/// The token never appears on argv, so it stays out of `/proc/<pid>/cmdline`
/// and the ssh debug log. If no token is available, falls back to an
/// unauthenticated clone (which works for public repos).
pub fn clone_git_repo(
    target: &SshTarget,
    github: Option<&GitHubAuth>,
    repo_url: &str,
) -> Result<()> {
    tracing::info!("Cloning {repo_url} into guest /workspace");

    let is_github = is_github_https_url(repo_url);
    let token = if is_github {
        resolve_clone_token(github, repo_url)?
    } else {
        None
    };

    let result = match token.as_deref() {
        Some(t) => clone_with_token(target, repo_url, t),
        None => clone_without_auth(target, repo_url),
    };

    let token_attempted = token.is_some();
    result.with_context(|| {
        if token_attempted {
            format!("Failed to clone {repo_url} in guest (host-resolved token rejected)")
        } else if is_github {
            format!(
                "Failed to clone {repo_url} in guest. \
                 If this is a private repo, configure a PAT with \
                 `coop github setup-pat --repo <owner/repo>`, run `gh auth login`, \
                 or set `GITHUB_TOKEN` on the host before `coop start --git-repo`."
            )
        } else {
            format!("Failed to clone {repo_url} in guest")
        }
    })?;

    tracing::info!("Repository cloned to /workspace/repo");
    Ok(())
}

/// If `strategy` is `Pat` mode and a `[github.pat."owner/repo"]` entry
/// exists for `repo_url`, return the matching slug. Otherwise `None`.
///
/// This is the sole "should the clone path use a configured PAT?" check.
/// Returning `None` directs [`resolve_clone_token`] to fall through to
/// [`host_github_token`] — preserving the pre-PAT behaviour for `Auto`,
/// `Env`, `Off`, and `Pat`-without-a-matching-entry.
fn clone_pat_slug(
    strategy: Option<&GitHubAuth>,
    repo_url: &str,
) -> Option<crate::github_repo::RepoSlug> {
    let strategy = strategy?;
    let GitHubAuth::Pat(_) = strategy else {
        return None;
    };
    let slug = crate::github_repo::parse_repo_slug_from_url(repo_url)?;
    strategy.pat_entry(&slug).is_some().then_some(slug)
}

/// Resolve the token to use for `git clone` of `repo_url`.
///
/// PAT mode with a matching entry wins — the user's per-repo intent
/// overrides the opportunistic host lookup. Every other configuration
/// (including `Pat` mode without a matching entry) falls back to
/// [`host_github_token`].
fn resolve_clone_token(strategy: Option<&GitHubAuth>, repo_url: &str) -> Result<Option<String>> {
    if let Some(slug) = clone_pat_slug(strategy, repo_url) {
        return resolve_pat_token(strategy, &slug).map(Some);
    }
    Ok(host_github_token())
}

fn clone_without_auth(target: &SshTarget, repo_url: &str) -> Result<()> {
    let cmd = format!(
        "sudo mkdir -p /workspace && \
         sudo chown $(whoami):$(whoami) /workspace && \
         git clone {} /workspace/repo && \
         echo 'Repository cloned to /workspace/repo'",
        shell_escape(repo_url),
    );
    target.exec(&cmd)
}

fn clone_with_token(target: &SshTarget, repo_url: &str, token: &str) -> Result<()> {
    let mut stdin = Vec::with_capacity(token.len() + 1);
    stdin.extend_from_slice(token.as_bytes());
    stdin.push(b'\n');
    target.exec_with_stdin(&build_clone_with_token_script(repo_url), stdin)
}

/// Build the remote shell script that reads a GitHub token from stdin and
/// uses it via a one-shot git credential helper to clone `repo_url`.
///
/// Separated from `clone_with_token` so the script template can be
/// unit-tested without spawning ssh.
fn build_clone_with_token_script(repo_url: &str) -> String {
    // The remote shell reads the token from stdin into GH_TOKEN, exports it
    // so the credential helper subshell inherits it, then clones with a
    // one-shot helper that returns the token to git. The single quotes
    // around the helper preserve `$GH_TOKEN` for expansion inside the
    // helper's subshell (not at script-parse time).
    format!(
        "set -eu\n\
         IFS= read -r GH_TOKEN\n\
         export GH_TOKEN\n\
         sudo mkdir -p /workspace\n\
         sudo chown \"$(whoami):$(whoami)\" /workspace\n\
         git -c credential.helper='!f() {{ echo username=x-access-token; echo \"password=$GH_TOKEN\"; }}; f' clone {url} /workspace/repo\n\
         echo 'Repository cloned to /workspace/repo'\n",
        url = shell_escape(repo_url),
    )
}

/// Returns true when `url` is an `https://github.com/...` URL with no userinfo.
///
/// Userinfo (`https://user:pass@github.com/...`) means the caller is supplying
/// their own credentials, so we leave the URL alone.
fn is_github_https_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    matches!(rest.split_once('/'), Some((host, _)) if host == "github.com")
}

/// Best-effort host-side GitHub token fallback for `git clone`.
///
/// Prefers `gh auth token` (uses the user's configured GitHub login),
/// falls back to `GITHUB_TOKEN`. Returns `None` when neither is available.
///
/// Used by [`resolve_clone_token`] when no `[github.pat."owner/repo"]`
/// entry matches the repo being cloned. The remaining `GitHubAuth` modes
/// (`Auto`, `Env`, `Off`) intentionally don't gate this fallback: the
/// token is consumed once via stdin in a one-shot `credential.helper`,
/// never enters the guest env, and the in-memory copy dies with the ssh
/// child — so the threat that the [`resolve_github_token`] gate defends
/// against (a persistent in-guest token visible to every guest process)
/// doesn't apply. `Pat` mode is the exception, handled upstream in
/// [`resolve_clone_token`], because a configured PAT entry is an explicit
/// per-repo intent statement and would be surprising to silently ignore.
fn host_github_token() -> Option<String> {
    select_host_token(
        gh_auth_token().as_deref(),
        std::env::var("GITHUB_TOKEN").ok().as_deref(),
    )
}

/// Pure picker: prefer the `gh` value, fall back to `GITHUB_TOKEN`.
/// Both inputs are trimmed; an all-whitespace or empty string is absent.
fn select_host_token(gh: Option<&str>, env: Option<&str>) -> Option<String> {
    let normalize = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    gh.and_then(normalize).or_else(|| env.and_then(normalize))
}

/// Run `gh auth token` and return the trimmed stdout, if any.
fn gh_auth_token() -> Option<String> {
    Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|t| !t.is_empty())
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
    fn hostname_accepts_valid_inputs() {
        for s in [
            "127.0.0.1",
            "172.16.0.2",
            "example.com",
            "host-1.local",
            "::1",
        ] {
            let h = Hostname::new(s).unwrap();
            assert_eq!(h.to_string(), s);
            assert_eq!(<Hostname as AsRef<str>>::as_ref(&h), s);
        }
    }

    #[test]
    fn hostname_rejects_empty() {
        assert!(Hostname::new("").is_err());
    }

    #[test]
    fn hostname_rejects_whitespace_and_control() {
        for s in ["host name", "host\tname", "host\nname", "name\0"] {
            assert!(Hostname::new(s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn hostname_rejects_non_ascii() {
        assert!(Hostname::new("hôst").is_err());
    }

    #[test]
    fn hostname_rejects_too_long() {
        let long = "a".repeat(MAX_HOSTNAME_LEN + 1);
        assert!(Hostname::new(long).is_err());
    }

    #[test]
    fn ssh_user_accepts_valid_inputs() {
        for s in ["ubuntu", "_user", "ec2-user", "u", "user_1", "a0"] {
            let u = SshUser::new(s).unwrap();
            assert_eq!(u.to_string(), s);
            assert_eq!(<SshUser as AsRef<str>>::as_ref(&u), s);
        }
    }

    #[test]
    fn ssh_user_rejects_empty() {
        assert!(SshUser::new("").is_err());
    }

    #[test]
    fn ssh_user_rejects_leading_digit_or_dash() {
        assert!(SshUser::new("0user").is_err());
        assert!(SshUser::new("-user").is_err());
    }

    #[test]
    fn ssh_user_rejects_uppercase_and_special() {
        for s in ["Ubuntu", "user!", "user.name", "user name"] {
            assert!(SshUser::new(s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn ssh_user_rejects_too_long() {
        let long = "a".repeat(MAX_SSH_USER_LEN + 1);
        assert!(SshUser::new(long).is_err());
    }

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
    fn managed_claude_settings_json_has_required_keys() {
        let body = managed_claude_settings_json();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let perms = parsed.get("permissions").unwrap();
        assert_eq!(
            perms.get("defaultMode").and_then(serde_json::Value::as_str),
            Some("bypassPermissions"),
        );
        assert_eq!(
            perms
                .get("skipDangerousModePermissionPrompt")
                .and_then(serde_json::Value::as_bool),
            Some(true),
        );
    }

    #[test]
    fn stage_codex_files_merges_mcp_servers_into_config() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("AGENTS.md"), "Global instructions").unwrap();
        std::fs::write(src.path().join("config.toml"), "model = \"gpt-5\"\n").unwrap();

        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "sentry".to_string(),
            McpServerDef::Http {
                url: url::Url::parse("https://mcp.sentry.dev/mcp").unwrap(),
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
    fn stage_codex_files_replaces_existing_mcp_servers_table() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(
            src.path().join("config.toml"),
            "model = \"gpt-5\"\n\
             \n\
             [mcp_servers.legacy]\n\
             command = \"legacy-server\"\n",
        )
        .unwrap();

        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "sentry".to_string(),
            McpServerDef::Http {
                url: url::Url::parse("https://mcp.sentry.dev/mcp").unwrap(),
                headers: std::collections::HashMap::new(),
            },
        );

        let staging = stage_codex_files(Some(src.path()), &servers).unwrap();
        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();

        assert!(config.contains("model = \"gpt-5\""));
        assert!(config.contains("[mcp_servers.sentry]"));
        assert!(
            !config.contains("legacy"),
            "replacement should drop pre-existing mcp_servers entries; got: {config}"
        );
    }

    #[test]
    fn stage_codex_files_copies_auth_json() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("auth.json"), "{\"access_token\":\"test\"}").unwrap();

        let staging =
            stage_codex_files(Some(src.path()), &std::collections::HashMap::new()).unwrap();
        assert_eq!(
            std::fs::read_to_string(staging.path().join("auth.json")).unwrap(),
            "{\"access_token\":\"test\"}"
        );
    }

    #[test]
    fn codex_bootstrap_needed_is_false_without_source_content_or_mcp() {
        let src = tempfile::TempDir::new().unwrap();
        assert!(!codex_bootstrap_needed(
            Some(src.path()),
            &std::collections::HashMap::new()
        ));
        assert!(!codex_bootstrap_needed(
            None,
            &std::collections::HashMap::new()
        ));
    }

    #[test]
    fn codex_bootstrap_needed_is_true_with_auth_json() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("auth.json"), "{\"access_token\":\"test\"}").unwrap();

        assert!(codex_bootstrap_needed(
            Some(src.path()),
            &std::collections::HashMap::new()
        ));
    }

    #[test]
    fn codex_missing_guest_cli_message_mentions_skip_and_rebuild_paths() {
        let msg = codex_missing_guest_cli_message();
        assert!(msg.contains("--no-agents"));
        assert!(msg.contains("--no-claude"));
        assert!(msg.contains("coop setup --rebuild"));
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

    #[test]
    fn is_github_https_url_matches_canonical_https() {
        assert!(is_github_https_url("https://github.com/owner/repo"));
        assert!(is_github_https_url("https://github.com/owner/repo.git"));
        assert!(is_github_https_url("https://github.com/"));
    }

    #[test]
    fn is_github_https_url_rejects_ssh_and_git_schemes() {
        assert!(!is_github_https_url("git@github.com:owner/repo.git"));
        assert!(!is_github_https_url("ssh://git@github.com/owner/repo"));
        assert!(!is_github_https_url("git://github.com/owner/repo"));
    }

    #[test]
    fn is_github_https_url_rejects_other_hosts() {
        assert!(!is_github_https_url("https://gitlab.com/owner/repo"));
        assert!(!is_github_https_url("https://example.com/github.com/repo"));
        assert!(!is_github_https_url("https://api.github.com/repos/x/y"));
    }

    #[test]
    fn is_github_https_url_rejects_userinfo() {
        // Caller is supplying their own credentials — leave URL alone.
        assert!(!is_github_https_url(
            "https://user:pass@github.com/owner/repo"
        ));
        assert!(!is_github_https_url("https://token@github.com/owner/repo"));
    }

    #[test]
    fn is_github_https_url_rejects_bare_host() {
        // No path component — not a clonable URL anyway, but make sure we
        // don't treat a non-URL like a match.
        assert!(!is_github_https_url("https://github.com"));
    }

    #[test]
    fn select_host_token_prefers_gh_over_env() {
        let pick = select_host_token(Some("gh-token"), Some("env-token"));
        assert_eq!(pick.as_deref(), Some("gh-token"));
    }

    #[test]
    fn select_host_token_falls_back_to_env_when_gh_absent() {
        let pick = select_host_token(None, Some("env-token"));
        assert_eq!(pick.as_deref(), Some("env-token"));
    }

    #[test]
    fn select_host_token_falls_back_to_env_when_gh_is_blank() {
        let pick = select_host_token(Some("   \n"), Some("env-token"));
        assert_eq!(pick.as_deref(), Some("env-token"));
    }

    #[test]
    fn select_host_token_trims_both_paths() {
        let pick = select_host_token(Some("  gh-token\n"), None);
        assert_eq!(pick.as_deref(), Some("gh-token"));
        let pick = select_host_token(None, Some("\tenv-token  "));
        assert_eq!(pick.as_deref(), Some("env-token"));
    }

    #[test]
    fn select_host_token_returns_none_when_both_absent_or_blank() {
        assert_eq!(select_host_token(None, None), None);
        assert_eq!(select_host_token(Some(""), Some("")), None);
        assert_eq!(select_host_token(Some(" "), Some("\n")), None);
    }

    #[test]
    fn build_clone_with_token_script_contains_required_pieces() {
        let script = build_clone_with_token_script("https://github.com/owner/repo.git");
        assert!(script.starts_with("set -eu\n"), "script: {script}");
        assert!(
            script.contains("IFS= read -r GH_TOKEN\n"),
            "missing read line: {script}"
        );
        assert!(
            script.contains("export GH_TOKEN\n"),
            "missing export line: {script}"
        );
        assert!(
            script.contains(
                "credential.helper='!f() { echo username=x-access-token; \
                 echo \"password=$GH_TOKEN\"; }; f'"
            ),
            "credential helper malformed: {script}"
        );
        assert!(
            script.contains(" clone 'https://github.com/owner/repo.git' /workspace/repo\n"),
            "missing escaped clone target: {script}"
        );
    }

    #[test]
    fn build_clone_with_token_script_escapes_repo_url() {
        // A URL with a single quote would otherwise break out of the
        // surrounding `'...'` argument. `shell_escape` must apply.
        let script = build_clone_with_token_script("https://github.com/o'wner/repo");
        // Embedded quote must be escaped via the standard `'\''` trick.
        assert!(
            script.contains("'https://github.com/o'\\''wner/repo'"),
            "single quote not escaped: {script}"
        );
    }

    fn pat_auth_with(entries: &[(&str, &str)]) -> GitHubAuth {
        let mut map = std::collections::BTreeMap::new();
        for (slug, token) in entries {
            let slug = crate::github_repo::RepoSlug::new(slug).unwrap();
            map.insert(
                slug,
                crate::config::PatEntry {
                    token: crate::config::Secret::new((*token).to_string()),
                },
            );
        }
        GitHubAuth::Pat(crate::config::PatConfig {
            entries: map,
            skip: Vec::new(),
        })
    }

    #[test]
    fn clone_pat_slug_matches_when_entry_exists() {
        let auth = pat_auth_with(&[("owner/repo", "github_pat_dummy")]);
        let expected = crate::github_repo::RepoSlug::new("owner/repo").unwrap();
        assert_eq!(
            clone_pat_slug(Some(&auth), "https://github.com/owner/repo.git"),
            Some(expected)
        );
    }

    #[test]
    fn clone_pat_slug_none_when_no_matching_entry() {
        let auth = pat_auth_with(&[("other/repo", "github_pat_dummy")]);
        assert!(clone_pat_slug(Some(&auth), "https://github.com/owner/repo.git").is_none());
    }

    #[test]
    fn clone_pat_slug_none_when_not_pat_mode() {
        // Even though the URL would parse to a slug, non-Pat modes never
        // route through the PAT branch — the host fallback handles them.
        for strategy in [
            None,
            Some(GitHubAuth::Auto),
            Some(GitHubAuth::Env),
            Some(GitHubAuth::Off),
        ] {
            assert!(
                clone_pat_slug(strategy.as_ref(), "https://github.com/owner/repo.git").is_none(),
                "expected None for strategy {:?}",
                strategy.as_ref().map(GitHubAuth::mode_name)
            );
        }
    }

    #[test]
    fn clone_pat_slug_none_for_non_github_url() {
        let auth = pat_auth_with(&[("owner/repo", "github_pat_dummy")]);
        // Non-GitHub URLs can't yield a slug, so PAT lookup never fires.
        assert!(clone_pat_slug(Some(&auth), "https://gitlab.com/owner/repo").is_none());
    }

    #[test]
    fn resolve_clone_token_returns_configured_pat_literal() {
        // Literal (non-`cmd:`) tokens pass through resolve_cmd_value
        // unchanged. The PAT is returned even if no host gh/env token
        // exists — the test for "PAT bypassed" is that it survives
        // independently of the host's environment.
        let auth = pat_auth_with(&[("owner/repo", "github_pat_literal")]);
        let token = resolve_clone_token(Some(&auth), "https://github.com/owner/repo.git").unwrap();
        assert_eq!(token.as_deref(), Some("github_pat_literal"));
    }

    // ── EnvForward Debug redaction ──────────────────────────

    #[test]
    fn env_forward_debug_redacts_all_values() {
        let mut env = EnvForward::default();
        env.set("ANTHROPIC_API_KEY", "sk-ant-secret-value");
        env.set("GITHUB_TOKEN", "ghp_secret_value");
        env.set("MYORG_INTERNAL", "internal-secret-blob");
        let debug = format!("{env:?}");
        for secret in [
            "sk-ant-secret-value",
            "ghp_secret_value",
            "internal-secret-blob",
        ] {
            assert!(
                !debug.contains(secret),
                "EnvForward Debug leaked value '{secret}': {debug}"
            );
        }
        // Keys are not secret — keep them in Debug output.
        for key in ["ANTHROPIC_API_KEY", "GITHUB_TOKEN", "MYORG_INTERNAL"] {
            assert!(
                debug.contains(key),
                "EnvForward Debug dropped key '{key}': {debug}"
            );
        }
    }

    #[test]
    fn env_forward_debug_empty() {
        let env = EnvForward::default();
        let debug = format!("{env:?}");
        assert!(debug.contains("EnvForward"));
    }

    // ── guest_env merge precedence ──────────────────────────

    /// Build a `CoopConfig` whose env-resolving inputs are all empty
    /// except `guest_env`. Defaults read `ANTHROPIC_API_KEY` /
    /// `OPENAI_API_KEY` from the process environment, which would make
    /// these tests flaky; clearing them keeps the assertions about
    /// `guest_env` precise.
    fn cfg_with_guest_env(entries: &[(&str, &str)]) -> CoopConfig {
        let mut cfg = CoopConfig::default();
        cfg.claude.api_key = None;
        cfg.claude.env_forward = Vec::new();
        cfg.codex.api_key = None;
        cfg.codex.env_forward = Vec::new();
        cfg.github = None;
        for (k, v) in entries {
            cfg.guest_env.insert(
                crate::guest_env_state::EnvVarName::new(k).unwrap(),
                (*v).to_string(),
            );
        }
        cfg
    }

    #[test]
    fn guest_env_entries_are_forwarded() {
        let cfg = cfg_with_guest_env(&[("RUST_LOG", "info"), ("MY_FLAG", "1")]);
        let env = prepare_env_forwarding(&cfg, None).unwrap();
        assert_eq!(
            env.as_envs().get("RUST_LOG").map(String::as_str),
            Some("info")
        );
        assert_eq!(env.as_envs().get("MY_FLAG").map(String::as_str), Some("1"));
    }

    #[test]
    fn guest_env_overrides_configured_api_key() {
        // Configured claude.api_key gets inserted first; guest_env with
        // the same name should win on conflict.
        let mut cfg = cfg_with_guest_env(&[("ANTHROPIC_API_KEY", "guest-env-wins")]);
        cfg.claude.api_key = Some(crate::config::Secret::new("from-claude-config".to_string()));

        let env = prepare_env_forwarding(&cfg, None).unwrap();
        assert_eq!(
            env.as_envs().get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("guest-env-wins"),
        );
    }

    #[test]
    fn guest_env_empty_value_is_preserved() {
        // Empty string is a legitimate value — distinguish "unset" from
        // "set to empty" so users can intentionally clear inherited vars.
        let cfg = cfg_with_guest_env(&[("EMPTY", "")]);
        let env = prepare_env_forwarding(&cfg, None).unwrap();
        assert_eq!(env.as_envs().get("EMPTY").map(String::as_str), Some(""));
    }
}
