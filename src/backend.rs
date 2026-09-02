use std::collections::BTreeMap;
#[cfg(target_os = "macos")]
use std::fs;
use std::num::{NonZeroU8, NonZeroU16};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use toml::Value as TomlValue;

use crate::cmd::Cmd;
use crate::config::{
    CodexAuthMode, ConfigDir, CoopConfig, GitHubAuth, ImageName, Instance, LocalModel,
    McpServerDef, NetworkConfig, VmMemory,
};
use crate::model_state::ModelState;
use crate::paths::{GuestPath, HostPath};
use crate::remote_command::RemoteCommand;
use crate::setup::SetupOptions;

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

// ── Stopped instance ──────────────────────────────────────────

/// Proof that an instance is currently *not* running. Construct via
/// [`VmBackend::as_stopped`] — like [`RunningInstance`], the
/// constructor is the single place that probes live state, so
/// operations taking a `StoppedInstance` can rely on the precondition
/// without re-checking.
///
/// No SSH target is carried: a stopped VM has nothing to connect to.
/// The field is private so a `StoppedInstance` cannot be forged; the
/// only way to obtain one is through a backend method that verified
/// the instance is not alive.
pub struct StoppedInstance {
    inst: Instance,
}

impl StoppedInstance {
    /// Mint a `StoppedInstance` after a successful live-state probe.
    ///
    /// Crate-private so only backend impls can construct one. Callers
    /// use [`VmBackend::as_stopped`] (which delegates here).
    pub(crate) fn new(inst: Instance) -> Self {
        Self { inst }
    }

    pub fn instance(&self) -> &Instance {
        &self.inst
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

impl From<std::net::Ipv4Addr> for Hostname {
    /// An IPv4 literal is always a valid hostname (printable ASCII, no
    /// whitespace, well under the length cap), so this conversion is
    /// infallible.
    fn from(ip: std::net::Ipv4Addr) -> Self {
        Self(ip.to_string())
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
    pub fn exec(&self, command: RemoteCommand) -> Result<()> {
        let cmd = command.into_string();
        let mut args = self.ssh_opts();
        args.push(self.addr());
        args.push(cmd.clone());

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
    pub fn exec_with_stdin(&self, command: RemoteCommand, stdin: Vec<u8>) -> Result<()> {
        let cmd = command.into_string();
        let mut args = self.ssh_opts();
        args.push(self.addr());
        args.push(cmd.clone());
        Cmd::new("ssh")
            .args(args)
            .stdin_input(stdin)
            .run()
            .with_context(|| format!("SSH command failed: {cmd}"))
    }

    /// Check if a command succeeds on the guest.
    pub fn exec_ok(&self, command: RemoteCommand) -> bool {
        let mut args = self.ssh_opts();
        args.push(self.addr());
        args.push(command.into_string());

        Command::new("ssh")
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Like `exec_ok` but uses connection multiplexing for fast retries.
    fn probe_ok_mux(&self, command: RemoteCommand) -> bool {
        let mut args = self.ssh_opts_mux();
        args.push(self.addr());
        args.push(command.into_string());

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
            if self.probe_ok_mux(RemoteCommand::new().literal("true")) {
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
    pub fn exec(&self, command: RemoteCommand) -> Result<()> {
        let cmd = command.into_string();
        let mut args = self.ssh_opts();
        args.push(self.target.addr());
        args.push(cmd.clone());

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
#[derive(serde::Serialize)]
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

/// Environmental preconditions for booting: the path-existence checks in
/// [`CoopConfig::validate`], run at the boot choke point on the freshest
/// filesystem state.
///
/// Every backend boot entry (`setup`, `create_and_start`, `start_existing`)
/// calls this first, so the check is unforgettable — a new lifecycle path
/// that reaches boot cannot skip it, and it sees the world as it is at boot
/// time rather than trusting a witness minted earlier. Warnings are dropped
/// here; they are surfaced once at the handler via
/// [`CoopConfig::validate_and_warn`].
pub fn boot_preflight(cfg: &CoopConfig) -> Result<()> {
    cfg.validate().map(drop)
}

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
    /// Resize a stopped instance's disk. Takes a [`StoppedInstance`]
    /// proof so the "must be stopped" precondition is enforced by the
    /// type system rather than a runtime guard.
    fn resize_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        new_size: crate::config::GiB,
    ) -> Result<()>;
    /// Change a stopped instance's memory and/or vCPU count. At least one
    /// of `mem`/`vcpus` is `Some` (the caller enforces this). The new
    /// value is written to the backend's authoritative artifact — the
    /// per-instance Firecracker JSON or the Lima `lima.yaml` — so it
    /// survives restarts rather than being reset from the global config.
    ///
    /// Firecracker writes the JSON atomically and does not boot the VM;
    /// Lima must `limactl start` to validate and apply the new spec, and
    /// restores the previous `lima.yaml` if that start fails. When
    /// `start_after` is true the instance is left running; otherwise it
    /// ends stopped (Lima stops it again after the validation boot).
    /// Gated on a [`StoppedInstance`] like [`Self::resize_disk`].
    fn set_machine_resources(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        mem: Option<VmMemory>,
        vcpus: Option<NonZeroU8>,
        start_after: bool,
    ) -> Result<()>;
    /// Save a stopped instance's filesystem as image `image` (the
    /// backend-specific disk artifacts only — the caller carries over
    /// the shared `template-config.json`). Takes a [`StoppedInstance`]
    /// proof so the "must be stopped" precondition (filesystem
    /// consistency) is enforced by the type system. Overwriting an
    /// existing image is the caller's decision, gated before this call.
    fn commit_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        image: &ImageName,
    ) -> Result<()>;
    /// Replace a stopped instance's disk with image `image`'s template,
    /// leaving the instance otherwise intact. The dual of
    /// [`Self::commit_disk`]; like it, gated on a [`StoppedInstance`].
    fn restore_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        image: &ImageName,
    ) -> Result<()>;
    fn is_running(&self, inst: &Instance) -> bool;
    /// Probe the live state of `inst`, returning a `RunningInstance`
    /// when it is up. This is the single chokepoint for "is this VM
    /// alive?" — call sites that need to operate on a running VM
    /// should ask via this method rather than open-coding the check.
    ///
    /// The two failure modes are kept distinct so callers probe once
    /// and never conflate them: `Ok(None)` means the instance is
    /// confirmed *not running*, while `Err` means the probe itself
    /// failed (e.g. `limactl list` errored, or the SSH target could
    /// not be built for a VM that *is* running). The `Instance` is
    /// consumed; callers that need it in the not-running branch can
    /// clone before calling.
    fn as_running(&self, cfg: &CoopConfig, inst: Instance) -> Result<Option<RunningInstance>>;
    /// Probe the live state of `inst` and return a [`StoppedInstance`]
    /// if it is not running. The dual of [`Self::as_running`] — used
    /// to gate operations (like `resize_disk`) that require the VM to
    /// be stopped. Returns `Err` when the instance is running.
    fn as_stopped(&self, inst: Instance) -> Result<StoppedInstance>;
    /// Render a human-readable status report for a running instance.
    /// Takes `&RunningInstance` so the precondition is part of the
    /// signature — status reporting probes the live guest (load,
    /// memory) and only makes sense while the VM is up.
    fn status(&self, cfg: &CoopConfig, running: &RunningInstance) -> Result<String>;
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
    /// The address the guest uses to reach a server running on the host,
    /// for rewriting local-model endpoints (see
    /// [`crate::network::rewrite_host_url`]). Firecracker guests route
    /// through the TAP gateway (`network.host_ip`); Lima injects
    /// `host.lima.internal`.
    fn guest_host_address(&self, network: &NetworkConfig) -> String;
    /// Whether mounts use live filesystem sharing (Lima/virtiofs)
    /// vs one-time sync (Firecracker/rsync).
    fn mounts_are_live(&self) -> bool;
    /// Whether `image` has its backend-specific build artifacts on disk.
    ///
    /// On Firecracker this means the template rootfs (`rootfs-template.ext4`).
    /// On Lima it means both the base disk (`lima-base.img`) and the start
    /// template (`lima-template.yaml`). Used by `coop quickstart` to skip
    /// `setup` when nothing needs building.
    fn image_is_built(&self, cfg: &CoopConfig, image: &ImageName) -> bool;
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
        boot_preflight(cfg)?;
        crate::setup::run(cfg, opts)
    }

    fn create_and_start(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        disk_gib: Option<crate::config::GiB>,
        mounts: &[crate::config::Mount],
    ) -> Result<()> {
        boot_preflight(cfg)?;
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
        boot_preflight(cfg)?;
        let vm = crate::vm::FirecrackerVm::new(cfg, inst);
        vm.configure()?;
        crate::network::setup_tap(&cfg.network, inst)?;
        let running = vm.start()?;
        running.wait_for_boot()
    }

    fn stop(&self, cfg: &CoopConfig, running: RunningInstance) -> Result<()> {
        let (inst, _target) = running.into_parts();
        let vm = crate::vm::FirecrackerVm::from_running_unchecked(cfg, &inst);
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
        stopped: &StoppedInstance,
        new_size: crate::config::GiB,
    ) -> Result<()> {
        let _ = cfg;
        crate::setup::resize_rootfs(stopped.instance(), new_size)
    }

    fn set_machine_resources(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        mem: Option<VmMemory>,
        vcpus: Option<NonZeroU8>,
        start_after: bool,
    ) -> Result<()> {
        let inst = stopped.instance();
        // Snapshot the prior spec so a failed validation boot can be rolled
        // back — otherwise `configure()` would re-read the rejected value on
        // every subsequent `coop start` and the instance would stay wedged.
        let previous = crate::vm::machine_resources(inst)?;
        crate::vm::set_machine_resources(inst, mem.map(VmMemory::get), vcpus)?;
        if start_after && let Err(e) = self.start_existing(cfg, inst) {
            if let Err(revert) =
                crate::vm::set_machine_resources(inst, Some(previous.0), Some(previous.1))
            {
                tracing::error!("Failed to revert machine config after failed start: {revert}");
            }
            return Err(e.context("Failed to start after reconfigure — reverted machine config"));
        }
        Ok(())
    }

    fn commit_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        image: &ImageName,
    ) -> Result<()> {
        crate::setup::commit_instance_rootfs(cfg, stopped.instance(), image)
    }

    fn restore_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        image: &ImageName,
    ) -> Result<()> {
        crate::setup::restore_instance_rootfs(cfg, stopped.instance(), image)
    }

    fn is_running(&self, inst: &Instance) -> bool {
        inst.is_running()
    }

    fn as_running(&self, cfg: &CoopConfig, inst: Instance) -> Result<Option<RunningInstance>> {
        if !inst.is_running() {
            return Ok(None);
        }
        let target = self.ssh_target(cfg, &inst)?;
        Ok(Some(RunningInstance::new(inst, target)))
    }

    fn as_stopped(&self, inst: Instance) -> Result<StoppedInstance> {
        if inst.is_running() {
            bail!(
                "Instance '{}' is running — stop it first with \
                 `coop stop {}`",
                inst.name,
                inst.name,
            );
        }
        Ok(StoppedInstance::new(inst))
    }

    fn status(&self, cfg: &CoopConfig, running: &RunningInstance) -> Result<String> {
        let vm = crate::vm::FirecrackerVm::from_running_unchecked(cfg, running.instance());
        vm.status()
    }

    fn stream_logs(
        &self,
        cfg: &CoopConfig,
        running: &RunningInstance,
        mode: LogMode,
    ) -> Result<()> {
        let vm = crate::vm::FirecrackerVm::from_running_unchecked(cfg, running.instance());
        vm.stream_logs(mode)
    }

    fn ssh_target(&self, cfg: &CoopConfig, inst: &Instance) -> Result<SshTarget> {
        let guest_user = persisted_guest_user(cfg, &inst.image);
        Ok(SshTarget {
            host: Hostname::from(inst.guest_ip()),
            port: cfg.ssh_port,
            user: SshUser::new(guest_user.as_str())?,
            key_path: cfg.ssh_key_path(),
        })
    }

    fn disk_path(&self, inst: &Instance) -> Result<PathBuf> {
        Ok(inst.rootfs_path())
    }

    fn guest_host_address(&self, network: &NetworkConfig) -> String {
        // The guest's default gateway is the bridge's host IP.
        network.host_ip.to_string()
    }

    fn mounts_are_live(&self) -> bool {
        false
    }

    fn image_is_built(&self, cfg: &CoopConfig, image: &ImageName) -> bool {
        cfg.template_path_for(image).exists()
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
        boot_preflight(cfg)?;
        crate::lima::setup(cfg, opts)
    }

    fn create_and_start(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        disk_gib: Option<crate::config::GiB>,
        mounts: &[crate::config::Mount],
    ) -> Result<()> {
        boot_preflight(cfg)?;
        crate::lima::create_and_start(cfg, inst, disk_gib, mounts)
    }

    fn start_existing(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        boot_preflight(cfg)?;
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
        stopped: &StoppedInstance,
        new_size: crate::config::GiB,
    ) -> Result<()> {
        crate::lima::resize_disk(cfg, stopped.instance(), new_size)
    }

    fn set_machine_resources(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        mem: Option<VmMemory>,
        vcpus: Option<NonZeroU8>,
        start_after: bool,
    ) -> Result<()> {
        crate::lima::set_machine_resources(
            cfg,
            stopped.instance(),
            mem.map(VmMemory::get),
            vcpus,
            start_after,
        )
    }

    fn commit_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        image: &ImageName,
    ) -> Result<()> {
        crate::lima::commit_disk(cfg, stopped.instance(), image)
    }

    fn restore_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        image: &ImageName,
    ) -> Result<()> {
        crate::lima::restore_disk(cfg, stopped.instance(), image)
    }

    fn is_running(&self, inst: &Instance) -> bool {
        crate::lima::is_running(inst)
    }

    fn as_running(&self, cfg: &CoopConfig, inst: Instance) -> Result<Option<RunningInstance>> {
        if !crate::lima::is_running(&inst) {
            return Ok(None);
        }
        let target = crate::lima::ssh_target(cfg, &inst)?;
        Ok(Some(RunningInstance::new(inst, target)))
    }

    fn as_stopped(&self, inst: Instance) -> Result<StoppedInstance> {
        if crate::lima::is_running(&inst) {
            bail!(
                "Instance '{}' is running — stop it first with \
                 `coop stop {}`",
                inst.name,
                inst.name,
            );
        }
        Ok(StoppedInstance::new(inst))
    }

    fn status(&self, cfg: &CoopConfig, running: &RunningInstance) -> Result<String> {
        crate::lima::status(cfg, running.instance())
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

    fn guest_host_address(&self, _network: &NetworkConfig) -> String {
        // Lima injects this hostname into the guest, resolving to the host.
        crate::lima::HOST_GATEWAY.to_string()
    }

    fn mounts_are_live(&self) -> bool {
        true
    }

    fn image_is_built(&self, cfg: &CoopConfig, image: &ImageName) -> bool {
        cfg.lima_base_path(image).exists() && cfg.lima_template_path(image).exists()
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
///
/// An unreadable or stale-format `workspace.json` (e.g. a pre-#147 file
/// that no longer deserializes) is logged at `WARN` with the loader's
/// migration hint before returning `None`, so the lost repo context is
/// diagnosable rather than silently degrading to "no token forwarded".
pub fn detect_instance_repo(
    inst: &crate::config::Instance,
) -> Option<crate::github_repo::RepoSlug> {
    use crate::workspace::WorkspaceSource;

    let state = crate::workspace::try_load_or_warn(
        inst,
        "GitHub repo detection is disabled (pat-mode tokens will not be forwarded)",
    )?;
    match &state.source {
        WorkspaceSource::GitRepo { url } => url.slug().cloned(),
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
    suppress_anthropic_key: bool,
    suppress_openai_key: bool,
) -> Result<EnvForward> {
    let claude = &cfg.claude;
    let codex = &cfg.codex;
    let codex_account_auth = codex.auth.uses_chatgpt_account();
    let suppress_openai_key = suppress_openai_key || codex_account_auth;
    // Only ever read under `suppress_openai_key`, so there is no "not
    // suppressed" case to name.
    let openai_suppression_reason = if codex_account_auth {
        "codex.auth = \"chatgpt\""
    } else {
        "proxy mode"
    };
    let mut env = EnvForward::default();

    // ANTHROPIC_API_KEY: prefer config, fall back to process env.
    // In proxy mode (issue #411) the raw key must never enter the guest —
    // the host-side proxy holds it and injects it upstream, so we forward
    // only the per-instance capability token via settings.json instead.
    if suppress_anthropic_key {
        tracing::debug!("proxy mode: not forwarding ANTHROPIC_API_KEY into the guest");
    } else if let Some(key) = &claude.api_key {
        let resolved = crate::config::resolve_cmd_value(key.expose())
            .context("Failed to resolve claude.api_key")?;
        env.set("ANTHROPIC_API_KEY", resolved);
    } else if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        env.set("ANTHROPIC_API_KEY", key);
    }

    // OPENAI_API_KEY: prefer config, fall back to process env. In proxy mode
    // the raw key stays on the host (injected upstream); Codex authenticates
    // to the proxy with the capability token via the provider `env_key`. In
    // ChatGPT account mode, forwarding is suppressed so Codex cannot silently
    // switch to API billing.
    if suppress_openai_key {
        tracing::debug!(
            "{openai_suppression_reason}: not forwarding OPENAI_API_KEY into the guest"
        );
    } else if let Some(key) = &codex.api_key {
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

    // In proxy mode the raw provider key must never reach the guest by ANY
    // path — not just the config/process-env fallback above, but also an
    // explicit `env_forward` or `guest_env` entry. Collect the suppressed
    // names so the loops below skip and loudly warn on such an entry.
    let mut suppressed: Vec<&str> = Vec::new();
    if suppress_anthropic_key {
        suppressed.push("ANTHROPIC_API_KEY");
    }
    if suppress_openai_key {
        suppressed.push("OPENAI_API_KEY");
    }
    let suppression_reason = |name: &str| {
        if name == "OPENAI_API_KEY" {
            openai_suppression_reason
        } else {
            "proxy mode"
        }
    };

    // User-specified env_forward vars from process environment
    for name in claude.env_forward.iter().chain(codex.env_forward.iter()) {
        if suppressed.contains(&name.as_str()) {
            let reason = suppression_reason(name.as_str());
            tracing::warn!("{reason}: ignoring env_forward entry '{name}'");
            continue;
        }
        if !env.contains(name.as_str())
            && let Ok(val) = std::env::var(name.as_str())
        {
            env.set(name.as_str(), val);
        }
    }

    // `guest_env` literals override anything resolved above (forwarded
    // host env vars, `claude.api_key`, etc.) so an explicit value beats
    // an inherited one. Warn on collision so the override is visible —
    // values are not logged (they may be secrets).
    for (name, value) in &cfg.guest_env {
        if suppressed.contains(&name.as_str()) {
            let reason = suppression_reason(name.as_str());
            tracing::warn!("{reason}: ignoring guest_env entry '{name}'");
            continue;
        }
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

    match session.exec(RemoteCommand::new().literal(command)) {
        Ok(()) => tracing::debug!("post_start hook completed"),
        Err(e) => tracing::warn!("post_start hook failed (continuing): {e}"),
    }
}

/// Bootstrap configured guest agents in the guest declaratively.
///
/// `guest_host` is the backend's guest-visible host address (from
/// [`VmBackend::guest_host_address`]), used to rewrite any local-model
/// endpoint so the guest can reach a server running on the host.
pub fn bootstrap_agents(
    session: &SshSession,
    cfg: &CoopConfig,
    inst: &crate::config::Instance,
    mode: BootMode,
    guest_host: &str,
) -> Result<()> {
    // GitHub auth is guest-global state. Refresh it once before either
    // agent bootstrap if a token is available.
    if session.env.contains("GITHUB_TOKEN") {
        tracing::info!("Configuring GitHub auth in guest");
        setup_github_auth(session)?;
    }

    bootstrap_claude(session, cfg, inst, mode, guest_host)?;
    bootstrap_codex(session, cfg, inst, mode, guest_host)?;

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
    guest_host: &str,
) -> Result<()> {
    let claude = &cfg.claude;
    let claude_bin = persisted_guest_user(cfg, &inst.image).claude_bin();

    if let BootMode::FirstBoot = mode {
        let needs_claude_cli = !claude.marketplaces.is_empty()
            || !claude.plugins.is_empty()
            || !claude.mcp_servers.is_empty();

        if needs_claude_cli
            && !session
                .target
                .exec_ok(RemoteCommand::new().literal("test -x ").arg(&claude_bin))
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
    // The same file carries the local-model `env` block when this VM is
    // in local mode, so the routing is refreshed on every boot and
    // survives stop/start.
    let model_state = ModelState::load_or_default(inst)?;
    // Proxy mode (issue #411): in remote mode with `[proxy]` configured,
    // start the host-side injecting proxy and point the guest at it. The
    // guest holds only the capability token; the real key stays on the host.
    // Fails closed — a resolution or spawn failure aborts the boot.
    let proxy = start_claude_proxy(inst, cfg, &model_state, &session.target)?;
    // Run the rest under a guard: if any step fails after the proxy is live,
    // tear it (and its reverse tunnel) down rather than orphaning a
    // credential-holding process after a failed boot. A clean bootstrap leaves
    // it running for the VM's lifetime. Mirrors `bootstrap_codex`.
    let result = (|| -> Result<()> {
        let local_env = claude_local_env(&model_state, cfg, guest_host, proxy.as_ref())?;
        write_managed_claude_settings(&session.target, &local_env)?;

        // Work around the onboarding wizard ignoring CLAUDE_CODE_OAUTH_TOKEN
        // (anthropics/claude-code#8938). Runs on every boot so a token added
        // before a restart still takes effect; a no-op once the flag is present
        // or when no OAuth token is forwarded.
        seed_claude_onboarding(session, &claude_bin)?;

        // Marketplaces, plugins, MCP servers — persisted on guest disk,
        // only install on first boot
        if let BootMode::FirstBoot = mode {
            // Compute delta: only install marketplaces/plugins not already
            // baked into the golden image
            let (missing_marketplaces, missing_plugins) = compute_plugin_delta(cfg, &inst.image);

            if !missing_marketplaces.is_empty() {
                install_marketplaces(session, &claude_bin, &missing_marketplaces)?;
            }

            if !missing_plugins.is_empty() {
                install_plugins(session, &claude_bin, &missing_plugins)?;
            }

            if !claude.mcp_servers.is_empty() {
                register_mcp_servers(session, &claude_bin, &claude.mcp_servers)?;
            }
        }

        tracing::info!("Claude Code bootstrap complete");
        Ok(())
    })();
    if result.is_err() && proxy.is_some() {
        crate::proxy::stop_provider(inst, crate::proxy::Provider::Anthropic);
    }
    result
}

/// Bootstrap Codex in the guest declaratively.
///
/// Codex uses `~/.codex/config.toml` for MCP registration and related
/// settings, so bootstrap writes allowlisted user config files and a
/// managed MCP section there when configured.
fn bootstrap_codex(
    session: &SshSession,
    cfg: &CoopConfig,
    inst: &crate::config::Instance,
    mode: BootMode,
    guest_host: &str,
) -> Result<()> {
    let mut model_state = ModelState::load_or_default(inst)?;
    ensure_codex_remote_auth_consistent(cfg, inst, &model_state)?;
    // Proxy mode (issue #411): in remote mode with `[proxy.openai]` (or a
    // per-VM override) configured, start the host-side injecting proxy and
    // point Codex at it. The guest holds only the capability token; the real
    // key stays on the host. Fails closed — a resolution/spawn failure aborts.
    let proxy = start_codex_proxy(inst, cfg, &model_state, &session.target)?;
    // Run the rest under a guard: if any step fails after the proxy is live,
    // tear it (and its reverse tunnel) down rather than orphaning a
    // credential-holding process after a failed boot. A clean bootstrap leaves
    // it running for the VM's lifetime.
    let result = (|| -> Result<()> {
        let codex = &cfg.codex;
        let source_dir = resolve_config_source_dir(&codex.config_dir, ".codex", "codex.config_dir");
        let local = codex_provider_table(&model_state, cfg, guest_host, proxy.as_ref())?;
        // coop_local is the provider id for both the local-model block and the
        // proxy block. Remember once coop has materialized it so a later
        // switch-off (to cloud, or after removing the config) reliably rewrites a
        // clean file that drops the block — even when no local endpoint resolves
        // (the proxy case). Persist so the memory survives the switch.
        if local.is_some() && !model_state.codex_materialized {
            model_state.codex_materialized = true;
            model_state.save(inst)?;
        }
        let manages_local = local.is_some()
            || model_state.resolved_codex(codex).is_some()
            || model_state.codex_materialized;
        let has_plugins = !codex.marketplaces.is_empty() || !codex.plugins.is_empty();
        let needs_codex = codex_bootstrap_needed(
            source_dir.as_deref(),
            &codex.mcp_servers,
            has_plugins,
            codex.auth,
        ) || manages_local
            || model_state.codex_keyring_materialized;

        if !needs_codex {
            return Ok(());
        }

        if codex.auth.uses_chatgpt_account() {
            ensure_codex_account_guest_support(&session.target)?;
        }

        if !session.target.exec_ok(
            RemoteCommand::new()
                .literal("test -x ")
                .arg(crate::guest::codex_bin()),
        ) {
            // The caller's guard tears down the proxy started above on this error.
            bail!("{}", codex_missing_guest_cli_message());
        }

        copy_codex_config(
            &session.target,
            source_dir.as_deref(),
            codex,
            local.as_ref(),
            manages_local,
            proxy.is_some(),
            model_state.codex_keyring_materialized,
        )?;

        // Same reasoning as `codex_materialized` above, for the keyring
        // credential-store key: remember that coop has written it so a later
        // switch back to `api_key` still rewrites a clean file that drops it,
        // even when nothing else would trigger a rewrite.
        //
        // Recorded only after `copy_codex_config` succeeded, unlike
        // `codex_materialized` — that flag records a fact already settled on
        // the host, this one a claim about the guest. Setting it earlier would
        // survive a failed bootstrap (a guest without Secret Service support,
        // a dropped scp) and then force a full config rewrite on every later
        // boot of a VM the guest key never reached — including the
        // `read_codex_plugin_state` round-trip, whose warn-and-continue on a
        // failed read drops the guest's own plugin tables.
        // Cleared as well as set, so it mirrors the guest rather than
        // latching: once the `api_key` rewrite above has actually dropped the
        // key, a stale `true` would force the very every-boot rewrite (and
        // `read_codex_plugin_state` round-trip) this comment warns about.
        let wants_keyring = codex.auth.uses_chatgpt_account();
        if wants_keyring != model_state.codex_keyring_materialized {
            model_state.codex_keyring_materialized = wants_keyring;
            model_state.save(inst)?;
        }

        if wants_keyring {
            remove_guest_codex_auth_json(&session.target);
        }

        // Marketplaces and plugins are persisted in the guest's config.toml and
        // plugin cache (and preserved across restarts by `copy_codex_config`), so
        // install only the delta not already baked into the golden image, and only
        // on first boot — marketplaces first, since `plugin add` resolves against a
        // configured marketplace.
        if let BootMode::FirstBoot = mode {
            let codex_bin = crate::guest::codex_bin();
            let (missing_marketplaces, missing_plugins) =
                compute_codex_plugin_delta(cfg, &inst.image);

            if !missing_marketplaces.is_empty() {
                install_codex_marketplaces(session, &codex_bin, &missing_marketplaces)?;
            }

            if !missing_plugins.is_empty() {
                install_codex_plugins(session, &codex_bin, &missing_plugins)?;
            }
        }

        match mode {
            BootMode::Restart => tracing::info!("Codex bootstrap refreshed"),
            BootMode::FirstBoot => tracing::info!("Codex bootstrap complete"),
        }

        Ok(())
    })();
    if result.is_err() && proxy.is_some() {
        crate::proxy::stop_provider(inst, crate::proxy::Provider::Openai);
    }
    result
}

fn codex_missing_guest_cli_message() -> &'static str {
    "Codex CLI is not installed in the guest.\n\
     The golden image may have been built before Codex support \
     was added, or the install failed silently.\n\
     If you want to skip Codex bootstrap for now, retry with \
     `--no-agents` (the `--no-claude` alias is deprecated).\n\
     Otherwise run `coop setup --rebuild` to rebuild the image."
}

pub fn codex_chatgpt_proxy_conflict_message() -> &'static str {
    "codex.auth = \"chatgpt\" conflicts with the effective OpenAI proxy for \
     this VM; Codex account auth uses ChatGPT workspace credentials, while \
     the OpenAI proxy uses an API key"
}

/// Reject the one Codex remote-auth combination `CoopConfig::validate` cannot
/// see: a per-VM `coop proxy` override that pairs an `OpenAI` upstream with
/// `codex.auth = "chatgpt"`. Every Codex entry point calls this so the failure
/// lands on Codex rather than on the whole session.
pub fn ensure_codex_remote_auth_consistent(
    cfg: &CoopConfig,
    inst: &crate::config::Instance,
    model_state: &ModelState,
) -> Result<()> {
    if cfg.codex.auth.uses_chatgpt_account()
        && model_state.mode == crate::model_state::ModelMode::Remote
        && crate::proxy_state::effective_upstream(inst, crate::proxy::Provider::Openai, &cfg.proxy)?
            .is_some()
    {
        bail!("{}", codex_chatgpt_proxy_conflict_message());
    }
    Ok(())
}

/// Delete a guest `~/.codex/auth.json` left behind by an earlier `api_key`
/// boot (or by a `codex login` run under `--no-agents`).
///
/// Dropping `auth.json` from the staged file set only stops coop *copying* a
/// new one — `copy_staged_to_guest` is additive and deletes nothing — so a VM
/// switched from `api_key` to `chatgpt` would keep a plaintext refresh token
/// on its guest disk, which is precisely what this mode exists to avoid.
///
/// Best-effort: the credential store is already the keyring by this point, so
/// a failure here leaves a stale file rather than breaking the boot. It is
/// loud about it, because the file is a credential.
fn remove_guest_codex_auth_json(target: &SshTarget) {
    if let Err(e) = target.exec(RemoteCommand::new().literal("rm -f ~/.codex/auth.json")) {
        tracing::warn!(
            "Could not remove a possible stale plaintext ~/.codex/auth.json from the \
             guest; Codex account auth stores credentials in the guest keyring, so any \
             such file is unused but still readable: {e:#}"
        );
    }
}

pub fn codex_keyring_not_configured_message() -> &'static str {
    "Codex ChatGPT account auth is configured, but this VM's guest \
     ~/.codex/config.toml does not select the keyring credential store.\n\
     coop writes that setting during agent bootstrap, so a VM started before \
     `auth = \"chatgpt\"` was set (or started with `--no-agents`) has not got \
     it yet, and Codex would write its account credentials to a plaintext \
     ~/.codex/auth.json in the guest instead.\n\
     Run `coop start` (without `--no-agents`) to bootstrap it."
}

/// Fail closed when the guest is not actually in keyring mode.
///
/// The guest wrapper gates on the guest's own `~/.codex/config.toml` — which
/// is what lets in-guest `codex-yolo` work — so if that file lacks the
/// setting the wrapper silently execs plain Codex and `codex login` writes a
/// plaintext token. `ensure_codex_account_guest_support` only proves the
/// *packages* are installed, and agent bootstrap only runs on start/up, so
/// enabling the mode against a running VM reaches exactly this gap.
pub fn ensure_codex_keyring_configured(target: &SshTarget) -> Result<()> {
    let configured = target.exec_ok(RemoteCommand::new().literal(
        "grep -Eq '^[[:space:]]*cli_auth_credentials_store[[:space:]]*=[[:space:]]*\"keyring\"' \
         ~/.codex/config.toml",
    ));
    if configured {
        return Ok(());
    }
    bail!("{}", codex_keyring_not_configured_message());
}

pub fn ensure_codex_account_guest_support(target: &SshTarget) -> Result<()> {
    let supported = target.exec_ok(
        RemoteCommand::new()
            .literal("test -x ")
            .arg(crate::guest::codex_account_bin())
            .literal(" && command -v dbus-run-session >/dev/null 2>&1")
            .literal(" && command -v gnome-keyring-daemon >/dev/null 2>&1")
            .literal(" && command -v secret-tool >/dev/null 2>&1"),
    );
    if supported {
        return Ok(());
    }

    bail!(
        "Codex ChatGPT account auth requires guest Secret Service support, \
         but this VM image does not have it.\n\
         Rebuild the image with `coop setup --rebuild` (or \
         `coop setup --image <name> --rebuild` for a named image).\n\
         A rebuild does not touch this VM's existing guest disk, and a \
         restart reuses it. To pick up the rebuilt image, either \
         `coop stop` then `coop restore <vm> --image <image>` (in place, \
         keeping the instance), or destroy and recreate the VM. \
         Alternatively, install `dbus-user-session`, `gnome-keyring`, and \
         `libsecret-tools` in the running guest by hand."
    );
}

/// Load the persisted guest user for an image, falling back to the
/// default `ubuntu` when no `template_config.json` exists yet (e.g.
/// the SSH target is being requested before setup has written the
/// config). Returning the default keeps pre-existing call sites
/// working unchanged on legacy images.
pub fn persisted_guest_user(cfg: &CoopConfig, image: &ImageName) -> crate::guest::GuestUser {
    crate::setup::TemplateConfig::load_for(cfg, image)
        .map(|tc| tc.guest_user)
        .unwrap_or_default()
}

/// Pure core of the `FirstBoot` install delta: the wanted marketplaces and
/// plugins that are not already baked into the golden image. A missing
/// baked list (legacy/orphaned image) is passed as an empty slice, so the
/// whole wanted set is treated as missing.
fn plugin_delta(
    wanted_marketplaces: &[String],
    wanted_plugins: &[String],
    baked_marketplaces: &[String],
    baked_plugins: &[String],
) -> (Vec<String>, Vec<String>) {
    fn missing(wanted: &[String], baked: &[String]) -> Vec<String> {
        wanted
            .iter()
            .filter(|w| !baked.contains(w))
            .cloned()
            .collect()
    }
    (
        missing(wanted_marketplaces, baked_marketplaces),
        missing(wanted_plugins, baked_plugins),
    )
}

/// Compute which Claude marketplaces and plugins are missing from the
/// golden image and need to be installed at start time.
fn compute_plugin_delta(cfg: &CoopConfig, image: &ImageName) -> (Vec<String>, Vec<String>) {
    let (baked_m, baked_p) = crate::setup::TemplateConfig::load_for(cfg, image)
        .ok()
        .map(|tc| (tc.marketplaces, tc.plugins))
        .unwrap_or_default();
    plugin_delta(
        &cfg.claude.marketplaces,
        &cfg.claude.plugins,
        &baked_m,
        &baked_p,
    )
}

/// Compute which Codex marketplaces and plugins are missing from the
/// golden image and need to be installed at start time.
fn compute_codex_plugin_delta(cfg: &CoopConfig, image: &ImageName) -> (Vec<String>, Vec<String>) {
    let (baked_m, baked_p) = crate::setup::TemplateConfig::load_for(cfg, image)
        .ok()
        .map(|tc| (tc.codex_marketplaces, tc.codex_plugins))
        .unwrap_or_default();
    plugin_delta(
        &cfg.codex.marketplaces,
        &cfg.codex.plugins,
        &baked_m,
        &baked_p,
    )
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
        .exec(RemoteCommand::new().literal("gh auth setup-git"))
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
    copy_staged_to_guest(target, &staged, ".claude", "Claude")
}

/// Copy every entry staged in `staged` into the guest's `~/<guest_subdir>/`,
/// creating the directory first. Files go via `scp_to`, subdirectories via
/// `scp_to_recursive`. An empty staging dir is a no-op (debug-logged).
/// `label` names the config in the log lines (e.g. "Claude", "Codex").
fn copy_staged_to_guest(
    target: &SshTarget,
    staged: &tempfile::TempDir,
    guest_subdir: &str,
    label: &str,
) -> Result<()> {
    let staging_path = staged.path();
    let has_entries = staging_path
        .read_dir()
        .context("Failed to read staging directory")?
        .next()
        .is_some();

    if !has_entries {
        tracing::debug!("No {label} config content to copy");
        return Ok(());
    }

    target.exec(RemoteCommand::new().literal(format!("mkdir -p ~/{guest_subdir}")))?;

    let guest_dir = GuestPath::new(format!("./{guest_subdir}"));
    for entry in std::fs::read_dir(staging_path).context("Failed to read staging directory")? {
        let entry = entry.context("Failed to read staging entry")?;
        let path = entry.path();
        let local = HostPath::new(&path);
        if path.is_dir() {
            target
                .scp_to_recursive(&local, &guest_dir)
                .with_context(|| format!("Failed to copy {} to guest", path.display()))?;
        } else {
            target
                .scp_to(&local, &guest_dir)
                .with_context(|| format!("Failed to copy {} to guest", path.display()))?;
        }
    }

    tracing::info!("Copied {label} config into guest");
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
///
/// When `local_env` is non-empty (the VM is in local-model mode), its
/// entries are written into the `env` block so Claude Code routes at the
/// local endpoint. Writing them here — rather than via SSH `SendEnv` —
/// makes the routing launch-independent: it applies to `claude` however
/// it is started in the guest, including a shell the user opened
/// themselves.
fn managed_claude_settings_json(local_env: &BTreeMap<String, String>) -> String {
    let mut settings = serde_json::json!({
        "permissions": {
            "defaultMode": "bypassPermissions",
            "skipDangerousModePermissionPrompt": true,
        }
    });
    if !local_env.is_empty() {
        settings["env"] = serde_json::json!(local_env);
    }
    settings.to_string()
}

/// Merge coop's managed permission keys into an existing `settings.json` body.
///
/// Only the managed `permissions` keys are forced; every other key Claude
/// Code writes is preserved (except the coop-managed `env` block — see below)
/// — notably `enabledPlugins` and `extraKnownMarketplaces`, which record
/// installed plugins and marketplaces.
/// Overwriting the whole file (the previous behavior) wiped those on every
/// boot, so plugins installed on first boot vanished from `/plugins` after a
/// stop/start cycle, since plugin install only runs on `BootMode::FirstBoot`.
///
/// An empty body is treated as an empty object. A body that is valid JSON but
/// not an object — or not valid JSON at all — is an error; the caller falls
/// back to writing managed defaults.
///
/// The local-model `env` block is coop-managed, not user state: when
/// `local_env` is non-empty (the VM is in local mode) it is written into the
/// `env` key, and when it is empty (remote mode) any existing `env` key is
/// removed, so switching back from a local model clears stale routing. This
/// mirrors [`managed_claude_settings_json`], which the fallback path uses.
fn merge_managed_claude_settings(
    existing: &str,
    local_env: &BTreeMap<String, String>,
) -> Result<String> {
    let mut root: serde_json::Value = if existing.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(existing)
            .context("existing ~/.claude/settings.json is not valid JSON")?
    };

    let root_obj = root
        .as_object_mut()
        .context("existing ~/.claude/settings.json is not a JSON object")?;

    let permissions = root_obj
        .entry("permissions")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let permissions_obj = permissions
        .as_object_mut()
        .context("`permissions` in ~/.claude/settings.json is not a JSON object")?;

    permissions_obj.insert(
        "defaultMode".to_string(),
        serde_json::Value::String("bypassPermissions".to_string()),
    );
    permissions_obj.insert(
        "skipDangerousModePermissionPrompt".to_string(),
        serde_json::Value::Bool(true),
    );

    if local_env.is_empty() {
        root_obj.remove("env");
    } else {
        root_obj.insert("env".to_string(), serde_json::json!(local_env));
    }

    serde_json::to_string(&root).context("Failed to serialize merged ~/.claude/settings.json")
}

/// Runs every VM startup and on `coop model` switches. Reads any existing
/// file and merges coop's managed `permissions` keys (and, in local-model
/// mode, the `env` block) into it via [`merge_managed_claude_settings`], so
/// plugin/marketplace state Claude Code persists in this file
/// (`enabledPlugins`, `extraKnownMarketplaces`) survives across reboots. A
/// file that cannot be parsed is replaced with managed defaults.
///
/// The write is staged to a temp file and renamed into place so a
/// `claude`/`coop ca` session reading the file concurrently (during a
/// live `coop model` switch) never observes a truncated file. The rename
/// is atomic because the temp file is in the same directory.
fn write_managed_claude_settings(
    target: &SshTarget,
    local_env: &BTreeMap<String, String>,
) -> Result<()> {
    target.exec(RemoteCommand::new().literal("mkdir -p ~/.claude"))?;

    // Read and merge as one fallible step so a read failure falls back to
    // managed defaults rather than aborting boot. `capture` decodes the file as
    // UTF-8, so a non-UTF-8 settings.json fails here; that is corrupt input,
    // handled the same as unparseable JSON below. The realistic Err is exactly
    // that corrupt file, which we want to reset anyway. A transient SSH read
    // failure (read and write are separate connections) is rare and back-to-
    // back with the write that follows; if that is ever shown to lose a
    // readable file in practice, distinguish a non-zero ssh exit from a decode
    // error and `?`-propagate the former.
    let merged = match target
        .capture("cat ~/.claude/settings.json 2>/dev/null || true")
        .and_then(|existing| merge_managed_claude_settings(&existing, local_env))
    {
        Ok(merged) => merged,
        Err(err) => {
            tracing::warn!(
                "Could not read or merge existing ~/.claude/settings.json ({err:#}); \
                 replacing it with managed defaults"
            );
            // Fallback is NOT preservation-safe: replacing the file with managed
            // defaults discards enabledPlugins/extraKnownMarketplaces — the very
            // keys this function exists to keep. Acceptable only because reaching
            // here means the existing file isn't a usable settings object (invalid
            // or non-UTF-8 bytes, malformed JSON, or a non-object `permissions`),
            // which coop never writes; a corrupt file is reset rather than merged.
            managed_claude_settings_json(local_env)
        }
    };

    target
        .exec_with_stdin(
            RemoteCommand::new().literal(
                "t=\"$(mktemp ~/.claude/settings.json.XXXXXX)\" && \
                 cat > \"$t\" && mv \"$t\" ~/.claude/settings.json",
            ),
            merged.into_bytes(),
        )
        .context("Failed to write managed ~/.claude/settings.json in guest")?;
    tracing::debug!("Wrote managed ~/.claude/settings.json to guest");
    Ok(())
}

/// Work around Claude Code's onboarding wizard ignoring a forwarded OAuth
/// token (anthropics/claude-code#8938).
///
/// Claude Code's TUI gates the theme/login wizard on `hasCompletedOnboarding`
/// in `~/.claude.json` and ignores `CLAUDE_CODE_OAUTH_TOKEN` for that check, so
/// subscription users land in the wizard even though the token authenticates
/// fine. coop never stages `~/.claude.json` (it is a sibling of the `.claude/`
/// dir, not inside it), so a fresh guest always lacks the flag. Seed it.
///
/// Only runs when the OAuth token is forwarded into the guest — API-key users
/// reach a working session without this, and credential-less guests must keep
/// the interactive login flow. Guarding on the forwarded env (not the host
/// process env) keys the workaround off wherever the token actually reaches the
/// guest, so it keeps working if a dedicated `claude.oauth_token` config field
/// later supersedes `env_forward`.
///
/// Idempotent: once the flag is present, the seeding `claude -p` call is
/// skipped. Delete this whole function when #8938 is fixed upstream.
fn seed_claude_onboarding(session: &SshSession, claude_bin: &GuestPath) -> Result<()> {
    if !session.env.contains("CLAUDE_CODE_OAUTH_TOKEN") {
        return Ok(());
    }
    let target = &session.target;

    if read_claude_json(target)
        .as_deref()
        .is_some_and(onboarding_marked_complete)
    {
        tracing::debug!("Claude onboarding already marked complete; skipping seed");
        return Ok(());
    }

    // Let Claude Code create the file itself so it carries whatever first-run
    // fields the installed version expects, then merge the flag in. Claude
    // writes ~/.claude.json on startup, before the API call `-p` makes
    // completes, so a timeout still leaves the file seeded — and the merge
    // below creates it from scratch if Claude failed to.
    seed_claude_json(session, claude_bin);

    let current = read_claude_json(target);
    let merged = claude_json_with_onboarding_complete(current.as_deref())?;
    write_claude_json(target, &merged)?;
    tracing::info!("Marked Claude onboarding complete in guest (~/.claude.json)");
    Ok(())
}

/// Read `~/.claude.json` from the guest, returning `None` when it is absent,
/// empty, or unreadable. A present file (even non-JSON) comes back verbatim.
///
/// Infallible by design: a read failure (e.g. a non-UTF-8 file, which
/// [`SshTarget::capture`] rejects) must not fail the boot over a cosmetic
/// workaround — it is treated as absent so the merge step rewrites a clean
/// file. A genuine connectivity problem surfaces later at the write.
fn read_claude_json(target: &SshTarget) -> Option<String> {
    match target.capture("cat ~/.claude.json 2>/dev/null || true") {
        Ok(raw) => (!raw.trim().is_empty()).then_some(raw),
        Err(e) => {
            tracing::debug!("Could not read ~/.claude.json ({e:#}); treating as absent");
            None
        }
    }
}

/// Run a throwaway `claude -p` so Claude Code seeds `~/.claude.json`. Best
/// effort: bounded by `timeout` and never fails the boot — the file may still
/// be absent afterwards (handled by the caller), and the API call timing out is
/// the expected case, not an error.
///
/// The 30s bound only needs to outlast Claude writing `~/.claude.json` on
/// startup, which happens well before the `-p` API call returns; it is a
/// safety net against a hung process, not a deadline for the call to succeed.
fn seed_claude_json(session: &SshSession, claude_bin: &GuestPath) {
    tracing::debug!("Seeding ~/.claude.json via `claude -p` to mark onboarding complete");
    let cmd = RemoteCommand::new()
        .literal("timeout 30 ")
        .arg(claude_bin)
        .literal(" -p ok >/dev/null 2>&1 || true");
    if let Err(e) = session.exec(cmd) {
        tracing::debug!("claude -p seed did not complete cleanly (continuing): {e}");
    }
}

/// Write `contents` to `~/.claude.json` in the guest, staged through a temp
/// file and renamed into place so a concurrent reader never sees a truncated
/// file (mirrors [`write_managed_claude_settings`]).
fn write_claude_json(target: &SshTarget, contents: &str) -> Result<()> {
    target
        .exec_with_stdin(
            RemoteCommand::new().literal(
                "t=\"$(mktemp ~/.claude.json.XXXXXX)\" && \
                 cat > \"$t\" && mv \"$t\" ~/.claude.json",
            ),
            contents.as_bytes().to_vec(),
        )
        .context("Failed to write ~/.claude.json in guest")
}

/// `true` when `claude_json` records completed onboarding. Tolerant: absent
/// flag, `false` flag, a non-object document, or invalid JSON all read as "not
/// complete" so the caller proceeds to seed.
fn onboarding_marked_complete(claude_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(claude_json)
        .ok()
        .as_ref()
        .and_then(|v| v.get("hasCompletedOnboarding"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// The contents to write to `~/.claude.json` so the TUI treats the guest as
/// onboarded. Preserves every existing key when `current` parses to a JSON
/// object; otherwise starts from an empty object. Always sets
/// `hasCompletedOnboarding: true`.
fn claude_json_with_onboarding_complete(current: Option<&str>) -> Result<String> {
    let mut obj = current
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|v| match v {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();
    obj.insert("hasCompletedOnboarding".to_string(), true.into());
    let rendered = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
        .context("Failed to serialize ~/.claude.json")?;
    Ok(format!("{rendered}\n"))
}

/// The `env` block for a Claude local endpoint, with its host URL
/// rewritten to be reachable from inside the guest. Empty when the VM is
/// not in local mode or no Claude endpoint resolves.
fn claude_local_env(
    state: &ModelState,
    cfg: &CoopConfig,
    guest_host: &str,
    proxy: Option<&crate::proxy::ProxyHandle>,
) -> Result<BTreeMap<String, String>> {
    // Local mode takes precedence over proxy mode: both rewrite the base URL,
    // and a VM switched to local should route at the user's model server.
    if let Some(ep) = local_endpoint(state, state.resolved_claude(&cfg.claude)) {
        let base_url = crate::network::rewrite_host_url(ep.host_url(), guest_host)?;
        return Ok(crate::model_state::claude_env_block(
            base_url.as_str(),
            ep.model(),
            &ep.auth_token_or_default(),
        ));
    }
    // Remote mode + proxy active → point Claude Code at the host-side proxy.
    if let Some(p) = proxy {
        return Ok(crate::model_state::claude_proxy_env_block(
            &p.base_url,
            p.capability_token.expose(),
        ));
    }
    Ok(BTreeMap::new())
}

/// Start the Anthropic credential proxy for this VM when proxy mode applies.
fn start_claude_proxy(
    inst: &crate::config::Instance,
    cfg: &CoopConfig,
    model_state: &ModelState,
    target: &SshTarget,
) -> Result<Option<crate::proxy::ProxyHandle>> {
    start_agent_proxy(
        inst,
        cfg,
        model_state,
        target,
        crate::proxy::Provider::Anthropic,
    )
}

/// Start the `OpenAI` credential proxy for this VM when proxy mode applies.
fn start_codex_proxy(
    inst: &crate::config::Instance,
    cfg: &CoopConfig,
    model_state: &ModelState,
    target: &SshTarget,
) -> Result<Option<crate::proxy::ProxyHandle>> {
    start_agent_proxy(
        inst,
        cfg,
        model_state,
        target,
        crate::proxy::Provider::Openai,
    )
}

/// Start one provider's credential proxy when proxy mode applies (remote model
/// mode + an effective upstream — per-VM override or `[proxy.<provider>]`
/// default), tearing down any stale proxy otherwise. Fails closed: a
/// resolution/spawn failure aborts the boot rather than falling back to
/// forwarding a raw key. Works on both backends — the proxy binds host
/// loopback and is reverse-tunnelled into the guest.
fn start_agent_proxy(
    inst: &crate::config::Instance,
    cfg: &CoopConfig,
    model_state: &ModelState,
    target: &SshTarget,
    provider: crate::proxy::Provider,
) -> Result<Option<crate::proxy::ProxyHandle>> {
    let effective = if model_state.mode == crate::model_state::ModelMode::Remote {
        crate::proxy_state::effective_upstream(inst, provider, &cfg.proxy)?
    } else {
        None
    };
    let Some(upstream) = effective else {
        // Not in proxy mode for this provider: ensure no proxy from a previous
        // boot (e.g. before a switch to local mode, or after the config was
        // removed) keeps running against this instance.
        crate::proxy::stop_provider(inst, provider);
        return Ok(None);
    };
    Ok(Some(crate::proxy::start_provider(
        inst, provider, &upstream, target,
    )?))
}

/// The Codex `config.toml` provider keys for this VM: the local-model block in
/// local mode, or the proxy block in remote+proxy mode, with the endpoint's
/// host URL rewritten for the guest. Local mode takes precedence over proxy
/// mode (mirrors [`claude_local_env`]). `None` when neither applies.
fn codex_provider_table(
    state: &ModelState,
    cfg: &CoopConfig,
    guest_host: &str,
    proxy: Option<&crate::proxy::ProxyHandle>,
) -> Result<Option<toml::Table>> {
    if let Some(ep) = local_endpoint(state, state.resolved_codex(&cfg.codex)) {
        let base_url = crate::network::rewrite_host_url(ep.host_url(), guest_host)?;
        return Ok(Some(crate::model_state::codex_local_config(
            base_url.as_str(),
            ep.model(),
        )));
    }
    if let Some(p) = proxy {
        return Ok(Some(crate::model_state::codex_proxy_config(&p.base_url)));
    }
    Ok(None)
}

/// Gate an already-resolved endpoint on the VM being in local mode. In
/// remote mode the materialization is intentionally empty so cloud
/// defaults apply.
fn local_endpoint<'a>(
    state: &ModelState,
    resolved: Option<&'a LocalModel>,
) -> Option<&'a LocalModel> {
    match state.mode {
        crate::model_state::ModelMode::Local => resolved,
        crate::model_state::ModelMode::Remote => None,
    }
}

fn copy_codex_config(
    target: &SshTarget,
    source_dir: Option<&Path>,
    codex: &crate::config::CodexConfig,
    local: Option<&toml::Table>,
    manages_local: bool,
    proxy_active: bool,
    keyring_materialized: bool,
) -> Result<()> {
    // The guest's config.toml holds the Codex CLI's installed `[marketplaces.*]`
    // / `[plugins.*]` tables. We only need to carry them across a *rewrite* of
    // that file, so read the guest config only when a rewrite will actually
    // happen — otherwise the file is left untouched and its state survives on
    // its own (and we skip a needless SSH round-trip).
    let preserved = if codex_config_needs_rewrite(
        source_dir,
        &codex.mcp_servers,
        local,
        manages_local,
        codex.auth,
        keyring_materialized,
    ) {
        read_codex_plugin_state(target)
    } else {
        None
    };

    let staged = stage_codex_files(
        source_dir,
        &codex.mcp_servers,
        local,
        manages_local,
        preserved.as_ref(),
        proxy_active,
        codex.auth,
        keyring_materialized,
    )
    .context("Failed to stage Codex config files")?;
    copy_staged_to_guest(target, &staged, ".codex", "Codex")
}

/// Read the guest's installed Codex marketplace/plugin tables for
/// preservation across a config.toml rewrite. Warns — rather than silently
/// proceeding — when the file is present but the read or parse fails, since
/// the impending rewrite would otherwise drop those tables without a trace.
/// Mirrors `merge_managed_claude_settings`'s warn-on-corrupt handling of
/// `~/.claude/settings.json`. A missing/empty file is the normal first-boot
/// case and yields `None` quietly.
///
/// Note the realistic failure here is a *transient* SSH read of an
/// otherwise-good file, not the corrupt file the Claude path deliberately
/// resets. Since plugin install runs only on `BootMode::FirstBoot`, if this
/// fails on a restart while a rewrite is otherwise triggered, the dropped
/// `[marketplaces.*]`/`[plugins.*]` are not reinstalled by that stop/start —
/// recovery requires recreating the instance (a fresh `FirstBoot`). The
/// warning is that signal.
fn read_codex_plugin_state(target: &SshTarget) -> Option<toml::Table> {
    match target.capture("cat ~/.codex/config.toml 2>/dev/null || true") {
        Ok(existing) => match extract_codex_plugin_state(&existing) {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(
                    "Could not parse guest ~/.codex/config.toml to preserve installed \
                     Codex marketplaces/plugins across the config refresh; they may be \
                     dropped and need reinstalling: {e:#}"
                );
                None
            }
        },
        Err(e) => {
            tracing::warn!(
                "Could not read guest ~/.codex/config.toml to preserve installed Codex \
                 marketplaces/plugins across the config refresh: {e:#}"
            );
            None
        }
    }
}

/// Guest-owned tables in `~/.codex/config.toml` that coop carries across its
/// own rewrite of that file.
///
/// `marketplaces`/`plugins` are the Codex CLI's installed marketplace
/// registrations and per-plugin enabled state. `projects` holds its
/// per-directory `trust_level` records — without it a user re-approves
/// workspace trust after every `coop stop`/`coop start`, which
/// `auth = "chatgpt"` would otherwise make the norm by forcing a rewrite on
/// every boot.
///
/// Deliberately *not* here: `model` / `model_provider`. Local-model routing
/// and proxy mode overwrite those on purpose, so preserving the guest's copy
/// would fight `coop model`.
const CODEX_PRESERVED_GUEST_TABLES: &[&str] = &["marketplaces", "plugins", "projects"];

/// Extract the guest-owned tables listed in [`CODEX_PRESERVED_GUEST_TABLES`]
/// from a guest `config.toml`, so coop can write them back verbatim when it
/// rewrites the file. Returns `Ok(None)` when the input is empty or carries
/// none of them, and `Err` when it is present but not valid TOML — so the
/// caller can warn rather than silently dropping the tables.
fn extract_codex_plugin_state(config_toml: &str) -> Result<Option<toml::Table>> {
    let parsed = toml::from_str::<TomlValue>(config_toml)
        .context("guest ~/.codex/config.toml is not valid TOML")?;
    let Some(table) = parsed.as_table() else {
        return Ok(None);
    };
    let mut preserved = toml::Table::new();
    for key in CODEX_PRESERVED_GUEST_TABLES {
        if let Some(value) = table.get(*key) {
            preserved.insert((*key).to_string(), value.clone());
        }
    }
    Ok((!preserved.is_empty()).then_some(preserved))
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
        ConfigDir::Custom(path) => path.to_path_buf(),
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

/// Allowlisted files copied verbatim from the host Codex config dir. In proxy
/// mode or `ChatGPT` account-auth mode `auth.json` is dropped (see
/// [`codex_allowed_files`]) so refreshable account tokens do not land on the
/// guest disk through a host-file copy.
const CODEX_ALLOWED_FILES: &[&str] = &["AGENTS.md", "auth.json"];

/// The Codex allowlist for this boot. In proxy mode `~/.codex/auth.json` is
/// dropped because Codex authenticates to the proxy with a capability token.
/// In `ChatGPT` account-auth mode it is dropped because Codex must use the guest
/// keyring instead of a host-copied file.
fn codex_allowed_files(proxy_active: bool, auth: CodexAuthMode) -> Vec<&'static str> {
    if proxy_active || auth.uses_chatgpt_account() {
        CODEX_ALLOWED_FILES
            .iter()
            .copied()
            .filter(|f| *f != "auth.json")
            .collect()
    } else {
        CODEX_ALLOWED_FILES.to_vec()
    }
}

/// Allowlisted directories copied recursively from the host Codex config dir.
const CODEX_ALLOWED_DIRS: &[&str] = &["prompts"];

/// Codex config file merged with managed MCP servers rather than copied verbatim.
const CODEX_CONFIG_FILE: &str = "config.toml";

/// Whether coop will rewrite the guest `~/.codex/config.toml` on this boot:
/// true when the host supplies a `config.toml` base, when managed
/// `mcp_servers` must be merged, when a local-model block is being written or
/// dropped (`manages_local`), when `auth = "chatgpt"` needs the keyring
/// credential store written, or when a previous boot wrote that key and this
/// one must drop it again (`keyring_materialized`). When false the guest file
/// is left untouched, so its installed plugin tables survive without coop
/// round-tripping them.
fn codex_config_needs_rewrite(
    source_dir: Option<&Path>,
    mcp_servers: &std::collections::HashMap<String, McpServerDef>,
    local: Option<&toml::Table>,
    manages_local: bool,
    auth: CodexAuthMode,
    keyring_materialized: bool,
) -> bool {
    source_dir.is_some_and(|path| path.join(CODEX_CONFIG_FILE).is_file())
        || !mcp_servers.is_empty()
        || local.is_some()
        || manages_local
        || auth.uses_chatgpt_account()
        || keyring_materialized
}

#[expect(
    clippy::too_many_arguments,
    reason = "each parameter is an independent, caller-resolved fact about this boot"
)]
fn stage_codex_files(
    source_dir: Option<&Path>,
    mcp_servers: &std::collections::HashMap<String, McpServerDef>,
    local: Option<&toml::Table>,
    manages_local: bool,
    preserved: Option<&toml::Table>,
    proxy_active: bool,
    auth: CodexAuthMode,
    keyring_materialized: bool,
) -> Result<tempfile::TempDir> {
    let staging = tempfile::TempDir::new().context("Failed to create staging directory")?;
    let allowed_files = codex_allowed_files(proxy_active, auth);

    let mut config = match source_dir {
        Some(path) => {
            stage_selected_files_into(path, staging.path(), &allowed_files, CODEX_ALLOWED_DIRS)
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

    if let Some(local) = local {
        let TomlValue::Table(root) = &mut config else {
            bail!("Codex {CODEX_CONFIG_FILE} must deserialize to a TOML table");
        };
        // Local-model routing overrides any model/provider the user's own
        // config.toml set; switching back to remote drops these keys
        // because the file is rebuilt from source each time.
        for (key, value) in local {
            root.insert(key.clone(), value.clone());
        }
    }

    {
        let TomlValue::Table(root) = &mut config else {
            bail!("Codex {CODEX_CONFIG_FILE} must deserialize to a TOML table");
        };
        if auth.uses_chatgpt_account() {
            root.insert(
                "cli_auth_credentials_store".to_string(),
                TomlValue::String("keyring".to_string()),
            );
        } else {
            // Explicit, not incidental: the host's own config.toml may set
            // this (the user may use keyring storage on the host too), and
            // copying it into an `api_key` guest would make the wrapper
            // demand a keyring password that mode does not need.
            root.remove("cli_auth_credentials_store");
        }
    }

    // The Codex CLI records installed marketplaces under `[marketplaces.*]`
    // and per-plugin enabled state under `[plugins.*]` in config.toml. Since
    // this rewrite would otherwise clobber them on every stop/start, drop any
    // `marketplaces`/`plugins` carried in from the *host* base (host-side
    // Codex state must not leak into the guest) and overlay the guest's own
    // tables read back in `copy_codex_config`. This mirrors the guest-side
    // `merge_managed_claude_settings` preservation of `enabledPlugins` /
    // `extraKnownMarketplaces`.
    {
        let TomlValue::Table(root) = &mut config else {
            bail!("Codex {CODEX_CONFIG_FILE} must deserialize to a TOML table");
        };
        for key in CODEX_PRESERVED_GUEST_TABLES {
            root.remove(*key);
        }
        if let Some(preserved) = preserved {
            for (key, value) in preserved {
                root.insert(key.clone(), value.clone());
            }
        }
    }

    // `manages_local` forces a rewrite even with no other content so that
    // switching back to remote drops a previously-written provider block.
    // `preserved` alone does not force a write: when nothing else triggers a
    // rewrite the guest's config.toml is left untouched, so its plugin state
    // survives without coop having to round-trip it. This must stay in sync
    // with the gate in `copy_codex_config` that decides whether to read the
    // guest config at all — hence the shared predicate.
    let should_write_config = codex_config_needs_rewrite(
        source_dir,
        mcp_servers,
        local,
        manages_local,
        auth,
        keyring_materialized,
    );

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
    has_plugins: bool,
    auth: CodexAuthMode,
) -> bool {
    codex_source_has_bootstrap_content(source_dir)
        || !mcp_servers.is_empty()
        || has_plugins
        || auth.uses_chatgpt_account()
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

/// Resolve a marketplace source for a guest `plugin marketplace add`.
///
/// A local absolute directory is copied into the guest's marketplace dir
/// and its guest-side path is returned; a URL / GitHub slug / `owner/repo`
/// shorthand is passed through unchanged. `made_dir` tracks whether the
/// guest marketplace dir has been created yet so it is only `mkdir -p`'d
/// once per install pass. Shared by the Claude and Codex marketplace
/// installers; `tool` (`"claude"` / `"codex"`) namespaces the copy dir so two
/// local marketplaces with the same directory basename — one per agent — do
/// not overwrite each other in the guest.
fn stage_marketplace_source(
    session: &SshSession,
    tool: &str,
    source: &str,
    made_dir: &mut bool,
) -> Result<String> {
    let local_path = Path::new(source);
    if !(local_path.is_absolute() && local_path.is_dir()) {
        return Ok(source.to_string());
    }

    let tool_dir = format!("{GUEST_MARKETPLACE_DIR}/{tool}");
    if !*made_dir {
        session
            .target
            .exec(RemoteCommand::new().literal(format!("mkdir -p {tool_dir}")))?;
        *made_dir = true;
    }
    let dir_name = local_path
        .file_name()
        .context("marketplace path has no directory name")?
        .to_string_lossy();
    let remote = GuestPath::new(format!("{tool_dir}/{dir_name}"));
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
    Ok(remote.to_string())
}

pub(crate) fn install_marketplaces(
    session: &SshSession,
    claude_bin: &GuestPath,
    marketplaces: &[String],
) -> Result<()> {
    let mut made_dir = false;
    for source in marketplaces {
        let guest_source = stage_marketplace_source(session, "claude", source, &mut made_dir)?;
        tracing::info!("Adding marketplace: {guest_source}");
        let cmd = RemoteCommand::new()
            .arg(claude_bin)
            .literal(" plugin marketplace add ")
            .arg(&guest_source)
            .literal(" --scope user");
        session
            .exec(cmd)
            .with_context(|| format!("Failed to add marketplace '{source}'"))?;
    }
    Ok(())
}

pub(crate) fn install_plugins(
    session: &SshSession,
    claude_bin: &GuestPath,
    plugins: &[String],
) -> Result<()> {
    for plugin in plugins {
        tracing::info!("Installing plugin: {plugin}");
        let cmd = RemoteCommand::new()
            .arg(claude_bin)
            .literal(" plugin install ")
            .arg(plugin)
            .literal(" -s user");
        session
            .exec(cmd)
            .with_context(|| format!("Failed to install plugin '{plugin}'"))?;
    }
    Ok(())
}

/// Register Codex marketplaces via `codex plugin marketplace add`.
///
/// Unlike Claude's `claude plugin marketplace add`, the Codex CLI takes
/// no `--scope` flag; the registration is written to `~/.codex/config.toml`
/// under `[marketplaces.<name>]`. Local directories are copied into the
/// guest first, mirroring [`install_marketplaces`].
pub(crate) fn install_codex_marketplaces(
    session: &SshSession,
    codex_bin: &GuestPath,
    marketplaces: &[String],
) -> Result<()> {
    let mut made_dir = false;
    for source in marketplaces {
        let guest_source = stage_marketplace_source(session, "codex", source, &mut made_dir)?;
        tracing::info!("Adding Codex marketplace: {guest_source}");
        let cmd = RemoteCommand::new()
            .arg(codex_bin)
            .literal(" plugin marketplace add ")
            .arg(&guest_source);
        session
            .exec(cmd)
            .with_context(|| format!("Failed to add Codex marketplace '{source}'"))?;
    }
    Ok(())
}

/// Install Codex plugins via `codex plugin add <plugin[@marketplace]>`.
///
/// The subcommand is `add` (not `install`, as for Claude), and there is
/// no `-s user` scope flag; enabled state is recorded in
/// `~/.codex/config.toml` under `[plugins.<name>]`.
pub(crate) fn install_codex_plugins(
    session: &SshSession,
    codex_bin: &GuestPath,
    plugins: &[String],
) -> Result<()> {
    for plugin in plugins {
        tracing::info!("Installing Codex plugin: {plugin}");
        let cmd = RemoteCommand::new()
            .arg(codex_bin)
            .literal(" plugin add ")
            .arg(plugin);
        session
            .exec(cmd)
            .with_context(|| format!("Failed to install Codex plugin '{plugin}'"))?;
    }
    Ok(())
}

fn register_mcp_servers(
    session: &SshSession,
    claude_bin: &GuestPath,
    servers: &std::collections::HashMap<String, McpServerDef>,
) -> Result<()> {
    for (name, def) in servers {
        tracing::info!("Registering MCP server: {name}");

        let mut resolved = def.clone();
        resolved.resolve_header_secrets("MCP server", name)?;

        let json = serde_json::to_string(&resolved)
            .context("Failed to serialize MCP server definition")?;
        let cmd = RemoteCommand::new()
            .arg(claude_bin)
            .literal(" mcp add-json -s user ")
            .arg(name)
            .literal(" ")
            .arg(&json);
        session
            .exec(cmd)
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
                 or set `GITHUB_TOKEN` on the host before starting the VM."
            )
        } else {
            format!("Failed to clone {repo_url} in guest")
        }
    })?;

    tracing::info!("Repository cloned to /workspace");
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
    let cmd = RemoteCommand::new()
        .literal(
            "sudo mkdir -p /workspace && \
             sudo chown $(whoami):$(whoami) /workspace && \
             git clone ",
        )
        .arg(repo_url)
        .literal(
            " /workspace && \
             echo 'Repository cloned to /workspace'",
        );
    target.exec(cmd)
}

fn clone_with_token(target: &SshTarget, repo_url: &str, token: &str) -> Result<()> {
    let mut stdin = Vec::with_capacity(token.len() + 1);
    stdin.extend_from_slice(token.as_bytes());
    stdin.push(b'\n');
    target.exec_with_stdin(build_clone_with_token_script(repo_url), stdin)
}

/// Build the remote shell script that reads a GitHub token from stdin and
/// uses it via a one-shot git credential helper to clone `repo_url`.
///
/// Separated from `clone_with_token` so the script template can be
/// unit-tested without spawning ssh.
fn build_clone_with_token_script(repo_url: &str) -> RemoteCommand {
    // The remote shell reads the token from stdin into GH_TOKEN, exports it
    // so the credential helper subshell inherits it, then clones with a
    // one-shot helper that returns the token to git. The single quotes
    // around the helper preserve `$GH_TOKEN` for expansion inside the
    // helper's subshell (not at script-parse time).
    RemoteCommand::new()
        .literal(
            "set -eu\n\
             IFS= read -r GH_TOKEN\n\
             export GH_TOKEN\n\
             sudo mkdir -p /workspace\n\
             sudo chown \"$(whoami):$(whoami)\" /workspace\n\
             git -c credential.helper='!f() { echo username=x-access-token; echo \"password=$GH_TOKEN\"; }; f' clone ",
        )
        .arg(repo_url)
        .literal(
            " /workspace\n\
             echo 'Repository cloned to /workspace'\n",
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
    fn boot_preflight_fails_on_missing_config_dir() {
        // The boot choke point must reject a config whose custom
        // claude.config_dir does not exist, before any VM cost (issue #404).
        let mut cfg = CoopConfig::default();
        cfg.claude.config_dir =
            ConfigDir::Custom(crate::config::ConfigPath::new("/nonexistent/claude-config"));
        let err = boot_preflight(&cfg).unwrap_err();
        assert!(err.to_string().contains("claude.config_dir"));
    }

    #[test]
    fn boot_preflight_passes_for_default_config() {
        // Default config has no custom config dirs or marketplace paths, so
        // the environmental (errors-only) check passes; warnings are dropped.
        boot_preflight(&CoopConfig::default()).unwrap();
    }

    #[test]
    fn claude_local_env_uses_proxy_block_in_remote_mode() {
        let state = ModelState {
            mode: crate::model_state::ModelMode::Remote,
            ..Default::default()
        };
        let cfg = CoopConfig::default();
        let proxy = crate::proxy::ProxyHandle {
            base_url: "http://172.16.0.1:8788".to_string(),
            capability_token: crate::config::Secret::new("cap-token".to_string()),
        };
        let env = claude_local_env(&state, &cfg, "172.16.0.1", Some(&proxy)).unwrap();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://172.16.0.1:8788");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "cap-token");
        // Proxy mode is transparent — no model pinning (unlike local mode).
        assert!(!env.contains_key("ANTHROPIC_MODEL"));
    }

    #[test]
    fn claude_local_env_prefers_local_over_proxy() {
        let ep = crate::config::LocalModel::new(
            url::Url::parse("http://localhost:11434").unwrap(),
            "qwen".to_string(),
            None,
        )
        .unwrap();
        let state = ModelState {
            mode: crate::model_state::ModelMode::Local,
            claude_endpoint: Some(ep),
            ..Default::default()
        };
        let cfg = CoopConfig::default();
        let proxy = crate::proxy::ProxyHandle {
            base_url: "http://172.16.0.1:8788".to_string(),
            capability_token: crate::config::Secret::new("cap-token".to_string()),
        };
        // Even with a proxy handle present, local mode wins.
        let env = claude_local_env(&state, &cfg, "172.16.0.1", Some(&proxy)).unwrap();
        assert_eq!(env["ANTHROPIC_MODEL"], "qwen");
        assert!(env["ANTHROPIC_BASE_URL"].starts_with("http://172.16.0.1:11434"));
    }

    #[test]
    fn claude_local_env_empty_without_proxy_or_local() {
        let env = claude_local_env(
            &ModelState::default(),
            &CoopConfig::default(),
            "172.16.0.1",
            None,
        )
        .unwrap();
        assert!(env.is_empty());
    }

    #[test]
    fn codex_provider_table_uses_proxy_block_in_remote_mode() {
        let state = ModelState {
            mode: crate::model_state::ModelMode::Remote,
            ..Default::default()
        };
        let cfg = CoopConfig::default();
        let proxy = crate::proxy::ProxyHandle {
            base_url: "http://127.0.0.1:9788".to_string(),
            capability_token: crate::config::Secret::new("cap-token".to_string()),
        };
        let table = codex_provider_table(&state, &cfg, "172.16.0.1", Some(&proxy))
            .unwrap()
            .unwrap();
        assert_eq!(
            table["model_provider"].as_str().unwrap(),
            crate::model_state::CODEX_LOCAL_PROVIDER
        );
        let provider = table["model_providers"][crate::model_state::CODEX_LOCAL_PROVIDER]
            .as_table()
            .unwrap();
        assert_eq!(
            provider["base_url"].as_str().unwrap(),
            "http://127.0.0.1:9788/v1"
        );
        // Proxy mode is transparent — no model pin (unlike local mode).
        assert!(!table.contains_key("model"));
    }

    #[test]
    fn codex_provider_table_prefers_local_over_proxy() {
        let ep = crate::config::LocalModel::new(
            url::Url::parse("http://localhost:11434/v1/").unwrap(),
            "qwen".to_string(),
            None,
        )
        .unwrap();
        let state = ModelState {
            mode: crate::model_state::ModelMode::Local,
            codex_endpoint: Some(ep),
            ..Default::default()
        };
        let cfg = CoopConfig::default();
        let proxy = crate::proxy::ProxyHandle {
            base_url: "http://127.0.0.1:9788".to_string(),
            capability_token: crate::config::Secret::new("cap-token".to_string()),
        };
        // Even with a proxy handle present, local mode wins (mirrors Claude).
        let table = codex_provider_table(&state, &cfg, "172.16.0.1", Some(&proxy))
            .unwrap()
            .unwrap();
        assert_eq!(table["model"].as_str().unwrap(), "qwen");
        let provider = table["model_providers"][crate::model_state::CODEX_LOCAL_PROVIDER]
            .as_table()
            .unwrap();
        assert!(
            provider["base_url"]
                .as_str()
                .unwrap()
                .starts_with("http://172.16.0.1:11434")
        );
    }

    #[test]
    fn codex_provider_table_none_without_proxy_or_local() {
        let table = codex_provider_table(
            &ModelState::default(),
            &CoopConfig::default(),
            "172.16.0.1",
            None,
        )
        .unwrap();
        assert!(table.is_none());
    }

    #[test]
    fn onboarding_marked_complete_detects_true_flag() {
        assert!(onboarding_marked_complete(
            r#"{"hasCompletedOnboarding": true, "theme": "dark"}"#
        ));
    }

    #[test]
    fn onboarding_marked_complete_false_when_flag_missing() {
        assert!(!onboarding_marked_complete(r#"{"theme": "dark"}"#));
    }

    #[test]
    fn onboarding_marked_complete_false_when_flag_false() {
        assert!(!onboarding_marked_complete(
            r#"{"hasCompletedOnboarding": false}"#
        ));
    }

    #[test]
    fn onboarding_marked_complete_false_on_invalid_or_non_object_json() {
        for raw in ["not json", "", "[]", "42", r#""hasCompletedOnboarding""#] {
            assert!(
                !onboarding_marked_complete(raw),
                "should not be complete: {raw:?}"
            );
        }
    }

    #[test]
    fn claude_json_with_onboarding_complete_preserves_existing_keys() {
        let rendered =
            claude_json_with_onboarding_complete(Some(r#"{"theme": "dark", "userID": "abc"}"#))
                .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["hasCompletedOnboarding"], serde_json::json!(true));
        assert_eq!(parsed["theme"], serde_json::json!("dark"));
        assert_eq!(parsed["userID"], serde_json::json!("abc"));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn claude_json_with_onboarding_complete_overwrites_false_flag() {
        let rendered =
            claude_json_with_onboarding_complete(Some(r#"{"hasCompletedOnboarding": false}"#))
                .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["hasCompletedOnboarding"], serde_json::json!(true));
    }

    #[test]
    fn claude_json_with_onboarding_complete_starts_fresh_when_absent() {
        let rendered = claude_json_with_onboarding_complete(None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed, serde_json::json!({"hasCompletedOnboarding": true}));
    }

    #[test]
    fn claude_json_with_onboarding_complete_replaces_non_object_or_invalid() {
        for raw in ["[1, 2, 3]", "garbage", "\"a string\""] {
            let rendered = claude_json_with_onboarding_complete(Some(raw)).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(
                parsed,
                serde_json::json!({"hasCompletedOnboarding": true}),
                "input {raw:?} should yield a fresh object"
            );
        }
    }

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

    fn temp_instance(dir: &std::path::Path) -> crate::config::Instance {
        crate::config::Instance {
            name: crate::config::InstanceName::new("test").unwrap(),
            index: crate::config::InstanceIndex::new(0).unwrap(),
            dir: dir.to_path_buf(),
            image: crate::config::ImageName::new("default").unwrap(),
        }
    }

    #[test]
    fn detect_instance_repo_none_when_state_missing() {
        let dir = tempfile::tempdir().unwrap();
        let inst = temp_instance(dir.path());
        assert!(detect_instance_repo(&inst).is_none());
    }

    #[test]
    fn detect_instance_repo_none_on_stale_format() {
        let dir = tempfile::tempdir().unwrap();
        let inst = temp_instance(dir.path());
        // Pre-#147 flat shape: `host_path` at the top level and a bare
        // `source` string. The current loader rejects this; detection must
        // degrade to `None` rather than panic.
        std::fs::write(
            inst.workspace_state_path(),
            r#"{"host_path":"/x","guest_path":"/workspace","source":"mount"}"#,
        )
        .unwrap();
        assert!(detect_instance_repo(&inst).is_none());
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
        let body = managed_claude_settings_json(&BTreeMap::new());
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
    fn managed_claude_settings_json_omits_env_block_when_remote() {
        let body = managed_claude_settings_json(&BTreeMap::new());
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            parsed.get("env").is_none(),
            "no env block expected in remote mode"
        );
    }

    #[test]
    fn managed_claude_settings_json_includes_local_env_block() {
        let env = crate::model_state::claude_env_block(
            "http://172.16.0.1:11434",
            "qwen2.5-coder",
            "coop-local",
        );
        let body = managed_claude_settings_json(&env);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let env_block = parsed.get("env").unwrap();
        assert_eq!(
            env_block
                .get("ANTHROPIC_BASE_URL")
                .and_then(serde_json::Value::as_str),
            Some("http://172.16.0.1:11434"),
        );
        assert_eq!(
            env_block
                .get("ANTHROPIC_MODEL")
                .and_then(serde_json::Value::as_str),
            Some("qwen2.5-coder"),
        );
        // Permissions survive alongside the injected env.
        assert!(parsed.get("permissions").is_some());
    }

    #[test]
    fn merge_managed_claude_settings_preserves_enabled_plugins() {
        let existing = r#"{
            "enabledPlugins": {"my-skill@my-market": true},
            "extraKnownMarketplaces": {"my-market": {"source": "/srv/m"}},
            "permissions": {"defaultMode": "default"}
        }"#;

        let merged = merge_managed_claude_settings(existing, &BTreeMap::new()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();

        assert_eq!(
            parsed
                .pointer("/enabledPlugins/my-skill@my-market")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "plugin enablement must survive the managed write",
        );
        assert!(
            parsed.get("extraKnownMarketplaces").is_some(),
            "marketplace registration must survive the managed write",
        );
        assert_eq!(
            parsed
                .pointer("/permissions/defaultMode")
                .and_then(serde_json::Value::as_str),
            Some("bypassPermissions"),
            "managed permissions must override an existing value",
        );
        assert_eq!(
            parsed
                .pointer("/permissions/skipDangerousModePermissionPrompt")
                .and_then(serde_json::Value::as_bool),
            Some(true),
        );
    }

    #[test]
    fn merge_managed_claude_settings_preserves_key_order() {
        // With serde_json's `preserve_order` feature the merge only touches
        // `permissions` and leaves every other key where the user had it,
        // instead of alphabetizing the whole file on every boot. Guards against
        // the feature being dropped from Cargo.toml.
        let existing = r#"{"zeta":1,"permissions":{"defaultMode":"default"},"alpha":2}"#;

        let merged = merge_managed_claude_settings(existing, &BTreeMap::new()).unwrap();

        let zeta = merged.find("\"zeta\"").unwrap();
        let alpha = merged.find("\"alpha\"").unwrap();
        assert!(
            zeta < alpha,
            "original key order must be preserved, not alphabetized: {merged}",
        );
    }

    #[test]
    fn merge_managed_claude_settings_preserves_enabled_plugins_in_local_mode() {
        // The local-model env block is injected without clobbering the
        // plugin/marketplace keys the merge exists to preserve.
        let existing = r#"{"enabledPlugins": {"my-skill@my-market": true}}"#;
        let env = crate::model_state::claude_env_block(
            "http://172.16.0.1:11434",
            "qwen2.5-coder",
            "coop-local",
        );

        let merged = merge_managed_claude_settings(existing, &env).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();

        assert_eq!(
            parsed
                .pointer("/enabledPlugins/my-skill@my-market")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "plugin enablement must survive the local-model env injection",
        );
        assert_eq!(
            parsed
                .pointer("/env/ANTHROPIC_MODEL")
                .and_then(serde_json::Value::as_str),
            Some("qwen2.5-coder"),
            "local-model env block must be merged in",
        );
    }

    #[test]
    fn merge_managed_claude_settings_replaces_existing_env_in_local_mode() {
        // Local mode replaces the whole env block rather than key-merging: coop
        // owns env (for local-model routing), so a stale/foreign env entry is
        // dropped, not preserved alongside the managed keys.
        let existing = r#"{"env": {"STALE": "1", "ANTHROPIC_MODEL": "old"}}"#;
        let env = crate::model_state::claude_env_block(
            "http://172.16.0.1:11434",
            "qwen2.5-coder",
            "coop-local",
        );

        let merged = merge_managed_claude_settings(existing, &env).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();

        assert!(
            parsed.pointer("/env/STALE").is_none(),
            "coop owns the env block; a foreign env entry must not survive: {merged}",
        );
        assert_eq!(
            parsed
                .pointer("/env/ANTHROPIC_MODEL")
                .and_then(serde_json::Value::as_str),
            Some("qwen2.5-coder"),
            "the managed env block must replace the prior one",
        );
    }

    #[test]
    fn merge_managed_claude_settings_clears_env_in_remote_mode() {
        // Switching back to a remote model (empty local_env) must drop a
        // previously written env block so stale routing does not linger.
        let existing = r#"{"env": {"ANTHROPIC_BASE_URL": "http://172.16.0.1:11434"}}"#;

        let merged = merge_managed_claude_settings(existing, &BTreeMap::new()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();

        assert!(
            parsed.get("env").is_none(),
            "remote mode must clear a previously injected env block: {merged}",
        );
    }

    #[test]
    fn merge_managed_claude_settings_empty_yields_defaults_only() {
        for body in ["", "   \n", "{}"] {
            let merged = merge_managed_claude_settings(body, &BTreeMap::new()).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
            assert_eq!(
                parsed
                    .pointer("/permissions/defaultMode")
                    .and_then(serde_json::Value::as_str),
                Some("bypassPermissions"),
                "empty body {body:?} must produce managed defaults",
            );
        }
    }

    #[test]
    fn merge_managed_claude_settings_rejects_non_object() {
        let empty = BTreeMap::new();
        assert!(merge_managed_claude_settings("not json", &empty).is_err());
        assert!(merge_managed_claude_settings("[1, 2, 3]", &empty).is_err());
        assert!(
            merge_managed_claude_settings(r#"{"permissions": "nope"}"#, &empty).is_err(),
            "a non-object `permissions` value must be rejected, not silently clobbered",
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

        let staging = stage_codex_files(
            Some(src.path()),
            &servers,
            None,
            false,
            None,
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();
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

        let staging = stage_codex_files(
            Some(src.path()),
            &servers,
            None,
            false,
            None,
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();
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

        let staging = stage_codex_files(
            Some(src.path()),
            &std::collections::HashMap::new(),
            None,
            false,
            None,
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(staging.path().join("auth.json")).unwrap(),
            "{\"access_token\":\"test\"}"
        );
    }

    #[test]
    fn stage_codex_files_drops_auth_json_in_proxy_mode() {
        // Issue #411 §7: in proxy mode the refreshable OpenAI subscription
        // token must not land on the guest disk.
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("auth.json"), "{\"access_token\":\"test\"}").unwrap();
        std::fs::write(src.path().join("AGENTS.md"), "hi").unwrap();

        let staging = stage_codex_files(
            Some(src.path()),
            &std::collections::HashMap::new(),
            None,
            false,
            None,
            true,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();
        assert!(
            !staging.path().join("auth.json").exists(),
            "auth.json must not be staged in proxy mode"
        );
        // Other allowlisted files are unaffected.
        assert!(staging.path().join("AGENTS.md").is_file());
    }

    #[test]
    fn stage_codex_files_drops_auth_json_in_chatgpt_mode() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("auth.json"), "{\"access_token\":\"test\"}").unwrap();

        let staging = stage_codex_files(
            Some(src.path()),
            &std::collections::HashMap::new(),
            None,
            false,
            None,
            false,
            CodexAuthMode::ChatGpt,
            false,
        )
        .unwrap();

        assert!(
            !staging.path().join("auth.json").exists(),
            "ChatGPT account auth must store credentials in keyring, not auth.json"
        );
        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert!(config.contains("cli_auth_credentials_store = \"keyring\""));
    }

    #[test]
    fn stage_codex_files_chatgpt_mode_writes_config_without_source() {
        let staging = stage_codex_files(
            None,
            &std::collections::HashMap::new(),
            None,
            false,
            None,
            false,
            CodexAuthMode::ChatGpt,
            false,
        )
        .unwrap();

        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert_eq!(config.trim(), "cli_auth_credentials_store = \"keyring\"");
    }

    #[test]
    fn stage_codex_files_drops_keyring_store_when_switching_back_to_api_key() {
        // Switching a VM from `chatgpt` to `api_key` with nothing else
        // configured used to leave `cli_auth_credentials_store = "keyring"`
        // behind in the guest, so the wrapper kept asking for a keyring
        // password that was no longer needed. `keyring_materialized` (from
        // `ModelState`) forces the rewrite that drops it.
        let staging = stage_codex_files(
            None,
            &std::collections::HashMap::new(),
            None,
            false,
            None,
            false,
            CodexAuthMode::ApiKey,
            true,
        )
        .unwrap();

        let path = staging.path().join("config.toml");
        assert!(
            path.is_file(),
            "a previously materialized keyring store must force a rewrite",
        );
        let config = std::fs::read_to_string(&path).unwrap();
        assert!(
            !config.contains("cli_auth_credentials_store"),
            "api_key mode must clear the keyring store, got: {config:?}",
        );
    }

    #[test]
    fn stage_codex_files_drops_a_host_supplied_keyring_store_in_api_key_mode() {
        // A user may use keyring storage on their *host* Codex too. Copying
        // that key into an `api_key` guest would make the wrapper demand a
        // keyring password the mode does not need, so the api_key path must
        // remove it rather than merely not add it.
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(
            src.path().join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\nmodel = \"gpt-5\"\n",
        )
        .unwrap();

        let staging = stage_codex_files(
            Some(src.path()),
            &std::collections::HashMap::new(),
            None,
            false,
            None,
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();

        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert!(
            !config.contains("cli_auth_credentials_store"),
            "api_key mode must drop a host-supplied keyring store, got: {config:?}",
        );
        assert!(
            config.contains("gpt-5"),
            "the rest of the host config must survive, got: {config:?}",
        );
    }

    #[test]
    fn extract_codex_plugin_state_preserves_project_trust_records() {
        // Codex writes `[projects."<dir>"] trust_level` when the user approves
        // a workspace. ChatGPT mode rewrites config.toml on every boot, so
        // without this the user re-approves trust after every restart.
        let guest = r#"
model = "gpt-5"

[projects."/workspace"]
trust_level = "trusted"

[marketplaces.acme]
url = "https://example.com/m"
"#;
        let preserved = extract_codex_plugin_state(guest).unwrap().unwrap();
        assert!(
            preserved.contains_key("projects"),
            "project trust records must survive coop's rewrite: {preserved:?}",
        );
        assert!(preserved.contains_key("marketplaces"));
        assert!(
            !preserved.contains_key("model"),
            "model is coop-managed (local/proxy routing overwrite it) and must not be preserved",
        );
    }

    #[test]
    fn stage_codex_files_drops_host_projects_but_keeps_guest_ones() {
        // Host-side Codex state must not leak into the guest, but the guest's
        // own trust records must come back — the same rule the marketplaces
        // and plugins tables already follow.
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(
            src.path().join("config.toml"),
            "[projects.\"/host/secret\"]\ntrust_level = \"trusted\"\n",
        )
        .unwrap();
        let mut preserved = toml::Table::new();
        let guest_projects =
            toml::from_str::<toml::Table>("[\"/workspace\"]\ntrust_level = \"trusted\"\n").unwrap();
        preserved.insert("projects".to_string(), toml::Value::Table(guest_projects));

        let staging = stage_codex_files(
            Some(src.path()),
            &std::collections::HashMap::new(),
            None,
            false,
            Some(&preserved),
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();

        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert!(
            config.contains("/workspace"),
            "the guest's own trust records must be written back: {config:?}",
        );
        assert!(
            !config.contains("/host/secret"),
            "host-side Codex project state must not reach the guest: {config:?}",
        );
    }

    #[test]
    fn codex_config_needs_rewrite_on_switch_away_from_chatgpt() {
        // The gate that lets the rewrite above happen at all: with no source,
        // no MCP servers, no local model and `api_key` auth, only the
        // materialized flag can force it.
        assert!(!codex_config_needs_rewrite(
            None,
            &std::collections::HashMap::new(),
            None,
            false,
            CodexAuthMode::ApiKey,
            false,
        ));
        assert!(codex_config_needs_rewrite(
            None,
            &std::collections::HashMap::new(),
            None,
            false,
            CodexAuthMode::ApiKey,
            true,
        ));
    }

    #[test]
    fn stage_codex_files_writes_local_provider_block() {
        // No source dir, no MCP servers: the local provider block alone
        // must still produce a config.toml.
        let local = crate::model_state::codex_local_config(
            "http://host.lima.internal:11434/v1/",
            "gpt-oss:120b",
        );
        let staging = stage_codex_files(
            None,
            &std::collections::HashMap::new(),
            Some(&local),
            true,
            None,
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();
        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert!(config.contains("model_provider = \"coop_local\""));
        assert!(config.contains("wire_api = \"responses\""));
        assert!(config.contains("http://host.lima.internal:11434/v1/"));
    }

    #[test]
    fn stage_codex_files_local_overrides_source_model() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("config.toml"), "model = \"gpt-5\"\n").unwrap();
        let local =
            crate::model_state::codex_local_config("http://172.16.0.1:11434/v1/", "qwen-local");
        let staging = stage_codex_files(
            Some(src.path()),
            &std::collections::HashMap::new(),
            Some(&local),
            true,
            None,
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();
        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert!(config.contains("model = \"qwen-local\""));
        assert!(
            !config.contains("gpt-5"),
            "local model must override source"
        );
    }

    #[test]
    fn stage_codex_files_remote_revert_drops_provider() {
        // Switching back to remote: no local table, but `manages_local`
        // forces a clean rewrite that omits the provider block.
        let staging = stage_codex_files(
            None,
            &std::collections::HashMap::new(),
            None,
            true,
            None,
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();
        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert!(
            !config.contains("coop_local"),
            "remote revert must drop the local provider; got: {config}"
        );
    }

    #[test]
    fn codex_bootstrap_needed_is_false_without_source_content_or_mcp() {
        let src = tempfile::TempDir::new().unwrap();
        assert!(!codex_bootstrap_needed(
            Some(src.path()),
            &std::collections::HashMap::new(),
            false,
            CodexAuthMode::ApiKey,
        ));
        assert!(!codex_bootstrap_needed(
            None,
            &std::collections::HashMap::new(),
            false,
            CodexAuthMode::ApiKey,
        ));
    }

    #[test]
    fn codex_bootstrap_needed_is_true_with_auth_json() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("auth.json"), "{\"access_token\":\"test\"}").unwrap();

        assert!(codex_bootstrap_needed(
            Some(src.path()),
            &std::collections::HashMap::new(),
            false,
            CodexAuthMode::ApiKey,
        ));
    }

    #[test]
    fn codex_bootstrap_needed_is_true_with_plugins() {
        // No source content and no MCP servers, but configured plugins alone
        // must still trigger bootstrap so the FirstBoot install runs.
        assert!(codex_bootstrap_needed(
            None,
            &std::collections::HashMap::new(),
            true,
            CodexAuthMode::ApiKey,
        ));
    }

    #[test]
    fn codex_bootstrap_needed_is_true_in_chatgpt_mode() {
        assert!(codex_bootstrap_needed(
            None,
            &std::collections::HashMap::new(),
            false,
            CodexAuthMode::ChatGpt,
        ));
    }

    #[test]
    fn extract_codex_plugin_state_round_trips_tables() {
        // Keys mirror real guest output: marketplaces are keyed by name,
        // plugins by `plugin@marketplace` (so the key needs quoting).
        let guest = "model = \"gpt-5\"\n\
             \n\
             [marketplaces.codex-plugins]\n\
             source = \"trailofbits/codex-plugins\"\n\
             \n\
             [plugins.\"my-lsp@codex-plugins\"]\n\
             enabled = true\n";
        let state = extract_codex_plugin_state(guest).unwrap().unwrap();
        assert!(state.contains_key("marketplaces"));
        assert!(state.contains_key("plugins"));
        // Only the two plugin tables are preserved, not unrelated keys.
        assert!(!state.contains_key("model"));
    }

    #[test]
    fn extract_codex_plugin_state_none_when_absent_or_empty() {
        assert!(extract_codex_plugin_state("").unwrap().is_none());
        assert!(
            extract_codex_plugin_state("model = \"gpt-5\"\n")
                .unwrap()
                .is_none()
        );
        // Present-but-invalid TOML is an error (so the caller can warn),
        // not a silent None that would drop installed plugin state.
        assert!(extract_codex_plugin_state("not = = valid").is_err());
    }

    #[test]
    fn stage_codex_files_preserves_guest_plugin_state() {
        // A host config.toml triggers a rewrite; the guest's installed
        // marketplace/plugin tables must survive it.
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("config.toml"), "model = \"gpt-5\"\n").unwrap();
        let preserved = extract_codex_plugin_state(
            "[marketplaces.codex-plugins]\nsource = \"trailofbits/codex-plugins\"\n\
             \n[plugins.\"my-lsp@codex-plugins\"]\nenabled = true\n",
        )
        .unwrap()
        .unwrap();
        let staging = stage_codex_files(
            Some(src.path()),
            &std::collections::HashMap::new(),
            None,
            false,
            Some(&preserved),
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();
        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert!(
            config.contains("[marketplaces.codex-plugins]"),
            "got: {config}"
        );
        assert!(
            config.contains("[plugins.\"my-lsp@codex-plugins\"]"),
            "got: {config}"
        );
        assert!(config.contains("model = \"gpt-5\""));
    }

    #[test]
    fn stage_codex_files_drops_host_base_plugin_state() {
        // Host config.toml carries its own marketplace/plugin tables (the user
        // runs Codex on the host too); with nothing preserved these must NOT
        // leak into the guest.
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(
            src.path().join("config.toml"),
            "model = \"gpt-5\"\n\
             \n[marketplaces.host-only]\nsource = \"someone/else\"\n\
             \n[plugins.host-plugin]\nenabled = true\n",
        )
        .unwrap();
        let staging = stage_codex_files(
            Some(src.path()),
            &std::collections::HashMap::new(),
            None,
            false,
            None,
            false,
            CodexAuthMode::ApiKey,
            false,
        )
        .unwrap();
        let config = std::fs::read_to_string(staging.path().join("config.toml")).unwrap();
        assert!(config.contains("model = \"gpt-5\""));
        assert!(
            !config.contains("host-only"),
            "host marketplace leaked: {config}"
        );
        assert!(
            !config.contains("host-plugin"),
            "host plugin leaked: {config}"
        );
    }

    #[test]
    fn plugin_delta_returns_wanted_minus_baked() {
        let wanted_m = vec!["a".to_string(), "b".to_string()];
        let wanted_p = vec!["p1@a".to_string(), "p2@b".to_string()];
        let (missing_m, missing_p) = plugin_delta(
            &wanted_m,
            &wanted_p,
            &["a".to_string()],
            &["p1@a".to_string()],
        );
        assert_eq!(missing_m, vec!["b".to_string()]);
        assert_eq!(missing_p, vec!["p2@b".to_string()]);

        // Empty baked (legacy/orphaned image): everything wanted is missing.
        let (all_m, all_p) = plugin_delta(&wanted_m, &wanted_p, &[], &[]);
        assert_eq!(all_m, wanted_m);
        assert_eq!(all_p, wanted_p);
    }

    #[test]
    fn compute_codex_plugin_delta_reads_codex_fields_not_claude() {
        // Pins the field wiring: the Codex delta must diff cfg.codex.* against
        // the template's codex_* lists, ignoring the Claude marketplaces/plugins
        // (backend.rs is mutation-excluded, so a copy-paste slip to a Claude
        // field would otherwise go uncaught).
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = CoopConfig {
            data_dir: crate::config::ConfigPath::new(tmp.path()),
            ..CoopConfig::default()
        };
        cfg.codex.marketplaces = vec!["m-baked".into(), "m-new".into()];
        cfg.codex.plugins = vec!["p-baked@m".into(), "p-new@m".into()];
        let image = ImageName::new("default").unwrap();

        // Bake one of each Codex entry — plus decoy Claude entries that must be
        // ignored — into the on-disk template config.
        std::fs::create_dir_all(cfg.image_dir(&image)).unwrap();
        let json = r#"{
            "version": 1,
            "created": "2026-01-01T00:00:00Z",
            "install_script_hash": "0000000000000000000000000000000000000000000000000000000000000000",
            "profiles": [],
            "extra_packages": [],
            "post_install_hash": null,
            "marketplaces": ["m-new"],
            "plugins": ["p-new@m"],
            "codex_marketplaces": ["m-baked"],
            "codex_plugins": ["p-baked@m"]
        }"#;
        std::fs::write(cfg.template_config_path_for(&image), json).unwrap();

        let (missing_m, missing_p) = compute_codex_plugin_delta(&cfg, &image);
        // Only the non-baked Codex entries remain. If the delta read the Claude
        // `marketplaces`/`plugins` (which bake "m-new"/"p-new@m"), those would
        // be filtered out and the assertion would fail.
        assert_eq!(missing_m, vec!["m-new".to_string()]);
        assert_eq!(missing_p, vec!["p-new@m".to_string()]);
    }

    #[test]
    fn codex_missing_guest_cli_message_mentions_skip_and_rebuild_paths() {
        let msg = codex_missing_guest_cli_message();
        assert!(msg.contains("--no-agents"));
        assert!(msg.contains("--no-claude"));
        assert!(msg.contains("coop setup --rebuild"));
    }

    #[test]
    fn codex_keyring_not_configured_message_names_the_recovery() {
        // This fires when the guest never got the keyring setting, so the
        // message has to name the step that writes it — otherwise the user is
        // told only that something is wrong.
        let msg = codex_keyring_not_configured_message();
        assert!(msg.contains("cli_auth_credentials_store") || msg.contains("keyring"));
        assert!(msg.contains("coop start"));
        assert!(msg.contains("--no-agents"));
        assert!(
            msg.contains("auth.json"),
            "the consequence — a plaintext token — is the reason to act",
        );
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
        let script =
            build_clone_with_token_script("https://github.com/owner/repo.git").into_string();
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
            script.contains(" clone 'https://github.com/owner/repo.git' /workspace\n"),
            "missing escaped clone target: {script}"
        );
    }

    #[test]
    fn build_clone_with_token_script_escapes_repo_url() {
        // A URL with a single quote would otherwise break out of the
        // surrounding `'...'` argument. `shell_escape` must apply.
        let script = build_clone_with_token_script("https://github.com/o'wner/repo").into_string();
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

    #[test]
    fn proxy_mode_suppresses_anthropic_key_forwarding() {
        // The crown-jewel invariant (issue #411): in proxy mode the raw
        // Anthropic key must never be forwarded into the guest, regardless of
        // config or the process environment.
        let mut cfg = CoopConfig::default();
        cfg.claude.api_key = Some(crate::config::Secret::new("sk-ant-realkey".to_string()));
        cfg.github = None;

        let suppressed = prepare_env_forwarding(&cfg, None, true, false).unwrap();
        assert!(
            !suppressed.contains("ANTHROPIC_API_KEY"),
            "raw Anthropic key leaked into guest env in proxy mode"
        );

        let forwarded = prepare_env_forwarding(&cfg, None, false, false).unwrap();
        assert!(
            forwarded.contains("ANTHROPIC_API_KEY"),
            "non-proxy mode should still forward the configured key"
        );
    }

    #[test]
    fn proxy_mode_suppresses_openai_key_forwarding() {
        // Same crown-jewel invariant for Codex/OpenAI (issue #411 slice 2): in
        // proxy mode the raw OpenAI key must never reach the guest.
        let mut cfg = CoopConfig::default();
        cfg.codex.api_key = Some(crate::config::Secret::new("sk-openai-realkey".to_string()));
        cfg.github = None;

        let suppressed = prepare_env_forwarding(&cfg, None, false, true).unwrap();
        assert!(
            !suppressed.contains("OPENAI_API_KEY"),
            "raw OpenAI key leaked into guest env in proxy mode"
        );

        let forwarded = prepare_env_forwarding(&cfg, None, false, false).unwrap();
        assert!(
            forwarded.contains("OPENAI_API_KEY"),
            "non-proxy mode should still forward the configured key"
        );
    }

    #[test]
    fn chatgpt_account_auth_suppresses_openai_key_forwarding() {
        let mut cfg = CoopConfig::default();
        cfg.codex.auth = CodexAuthMode::ChatGpt;
        cfg.codex.api_key = Some(crate::config::Secret::new("sk-openai-realkey".to_string()));
        cfg.github = None;

        let env = prepare_env_forwarding(&cfg, None, false, false).unwrap();
        assert!(
            !env.contains("OPENAI_API_KEY"),
            "ChatGPT account auth must not forward an OpenAI API key"
        );
    }

    // ── ensure_codex_remote_auth_consistent ─────────────────

    fn auth_check_cfg(auth: CodexAuthMode, openai: bool, anthropic: bool) -> CoopConfig {
        let upstream = || crate::config::ProxyUpstream {
            credential: crate::config::Secret::new("sk-upstream".to_string()),
            auth: crate::config::ProxyAuthScheme::Bearer,
        };
        let mut cfg = CoopConfig::default();
        cfg.codex.auth = auth;
        cfg.proxy.openai = openai.then(upstream);
        cfg.proxy.anthropic = anthropic.then(upstream);
        cfg
    }

    #[test]
    fn codex_remote_auth_rejects_chatgpt_with_openai_upstream() {
        // The tempdir holds no `proxy.json`, so `effective_upstream` resolves
        // purely from the `[proxy]` config these fixtures build — the same
        // holds for the three tests below.
        let tmp = tempfile::TempDir::new().unwrap();
        let inst = temp_instance(tmp.path());
        let cfg = auth_check_cfg(CodexAuthMode::ChatGpt, true, false);
        let state = crate::model_state::ModelState::default();
        assert_eq!(state.mode, crate::model_state::ModelMode::Remote);

        let err = ensure_codex_remote_auth_consistent(&cfg, &inst, &state).unwrap_err();
        assert_eq!(err.to_string(), codex_chatgpt_proxy_conflict_message());
    }

    #[test]
    fn codex_remote_auth_rejects_chatgpt_with_per_vm_openai_override() {
        // The case this guard exists for, and the only one that reaches it in
        // production: `CoopConfig::validate` already rejects a config-level
        // `[proxy.openai]` alongside `auth = "chatgpt"`, so the pairing can
        // only survive a validated parse when it comes from a per-VM `coop
        // proxy --vm` override in `<inst.dir>/proxy.json`. Config `[proxy]` is
        // empty here, so only the on-disk override can trip the bail.
        let tmp = tempfile::TempDir::new().unwrap();
        let inst = temp_instance(tmp.path());
        crate::proxy_state::ProxyState {
            openai: Some(crate::config::ProxyUpstream {
                credential: crate::config::Secret::new("sk-per-vm".to_string()),
                auth: crate::config::ProxyAuthScheme::Bearer,
            }),
            ..Default::default()
        }
        .save(&inst)
        .unwrap();

        let cfg = auth_check_cfg(CodexAuthMode::ChatGpt, false, false);
        assert!(
            cfg.proxy.openai.is_none(),
            "the override must be the source"
        );

        let err = ensure_codex_remote_auth_consistent(
            &cfg,
            &inst,
            &crate::model_state::ModelState::default(),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), codex_chatgpt_proxy_conflict_message());
    }

    #[test]
    fn codex_remote_auth_allows_chatgpt_with_openai_upstream_in_local_mode() {
        // Local mode never starts the OpenAI proxy, so the pair is not a
        // conflict — this is the `model_state` term of the guard.
        let tmp = tempfile::TempDir::new().unwrap();
        let inst = temp_instance(tmp.path());
        let cfg = auth_check_cfg(CodexAuthMode::ChatGpt, true, false);
        let state = crate::model_state::ModelState {
            mode: crate::model_state::ModelMode::Local,
            ..Default::default()
        };

        assert!(
            ensure_codex_remote_auth_consistent(&cfg, &inst, &state).is_ok(),
            "local mode must not trip the proxy conflict",
        );
    }

    #[test]
    fn codex_remote_auth_allows_api_key_with_openai_upstream() {
        // The `uses_chatgpt_account` term: API-key auth is exactly what the
        // OpenAI proxy is for.
        let tmp = tempfile::TempDir::new().unwrap();
        let inst = temp_instance(tmp.path());
        let cfg = auth_check_cfg(CodexAuthMode::ApiKey, true, false);

        assert!(
            ensure_codex_remote_auth_consistent(
                &cfg,
                &inst,
                &crate::model_state::ModelState::default(),
            )
            .is_ok(),
            "api_key auth with an OpenAI upstream is the supported pairing",
        );
    }

    #[test]
    fn codex_remote_auth_allows_chatgpt_with_anthropic_only_upstream() {
        // The provider argument: a Claude-only proxy says nothing about Codex.
        let tmp = tempfile::TempDir::new().unwrap();
        let inst = temp_instance(tmp.path());
        let cfg = auth_check_cfg(CodexAuthMode::ChatGpt, false, true);

        assert!(
            ensure_codex_remote_auth_consistent(
                &cfg,
                &inst,
                &crate::model_state::ModelState::default(),
            )
            .is_ok(),
            "an Anthropic-only proxy must not trip the Codex guard",
        );
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
        let env = prepare_env_forwarding(&cfg, None, false, false).unwrap();
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

        let env = prepare_env_forwarding(&cfg, None, false, false).unwrap();
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
        let env = prepare_env_forwarding(&cfg, None, false, false).unwrap();
        assert_eq!(env.as_envs().get("EMPTY").map(String::as_str), Some(""));
    }

    #[test]
    fn proxy_mode_suppresses_anthropic_key_from_guest_env() {
        // The crown-jewel invariant must hold even when the raw key is listed
        // explicitly in `guest_env` — the suppression left the name absent, so
        // the collision warning never fires and it would otherwise forward
        // silently.
        let cfg = cfg_with_guest_env(&[("ANTHROPIC_API_KEY", "sk-ant-realkey")]);
        let suppressed = prepare_env_forwarding(&cfg, None, true, false).unwrap();
        assert!(
            !suppressed.contains("ANTHROPIC_API_KEY"),
            "guest_env re-injected the raw Anthropic key in proxy mode"
        );
        // Non-proxy mode still forwards the explicit guest_env value.
        let forwarded = prepare_env_forwarding(&cfg, None, false, false).unwrap();
        assert_eq!(
            forwarded
                .as_envs()
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("sk-ant-realkey")
        );
    }

    #[test]
    fn proxy_mode_suppresses_openai_key_from_guest_env() {
        let cfg = cfg_with_guest_env(&[("OPENAI_API_KEY", "sk-openai-realkey")]);
        let suppressed = prepare_env_forwarding(&cfg, None, false, true).unwrap();
        assert!(
            !suppressed.contains("OPENAI_API_KEY"),
            "guest_env re-injected the raw OpenAI key in proxy mode"
        );
        let forwarded = prepare_env_forwarding(&cfg, None, false, false).unwrap();
        assert_eq!(
            forwarded
                .as_envs()
                .get("OPENAI_API_KEY")
                .map(String::as_str),
            Some("sk-openai-realkey")
        );
    }

    #[test]
    fn chatgpt_account_auth_suppresses_openai_key_from_guest_env() {
        let mut cfg = cfg_with_guest_env(&[("OPENAI_API_KEY", "sk-openai-realkey")]);
        cfg.codex.auth = CodexAuthMode::ChatGpt;

        let env = prepare_env_forwarding(&cfg, None, false, false).unwrap();
        assert!(
            !env.contains("OPENAI_API_KEY"),
            "guest_env must not re-inject an OpenAI API key in ChatGPT account mode"
        );
    }

    #[test]
    fn proxy_mode_suppresses_anthropic_key_from_env_forward() {
        // The third raw-key entry path: an explicit env_forward entry resolved
        // from the host process env. Serialize the env mutation and restore the
        // prior value so parallel tests are unaffected.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let prior = std::env::var("ANTHROPIC_API_KEY").ok();
        // SAFETY: this is the only test that mutates ANTHROPIC_API_KEY, it holds
        // ENV_LOCK while doing so, and it restores the prior value before
        // returning. No coop test asserts on a process-inherited
        // ANTHROPIC_API_KEY, so a transient read by another thread is benign.
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-from-host-env") };

        let mut cfg = CoopConfig::default();
        cfg.claude.api_key = None;
        cfg.codex.api_key = None;
        cfg.github = None;
        cfg.claude.env_forward =
            vec![crate::guest_env_state::EnvVarName::new("ANTHROPIC_API_KEY").unwrap()];

        let suppressed = prepare_env_forwarding(&cfg, None, true, false).unwrap();
        let forwarded = prepare_env_forwarding(&cfg, None, false, false).unwrap();

        // SAFETY: same lock still held; restore the environment to its prior state.
        unsafe {
            match &prior {
                Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
        }

        assert!(
            !suppressed.contains("ANTHROPIC_API_KEY"),
            "env_forward re-injected the raw Anthropic key in proxy mode"
        );
        assert_eq!(
            forwarded
                .as_envs()
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("sk-ant-from-host-env"),
            "non-proxy mode should still forward an env_forward entry"
        );
    }
}
