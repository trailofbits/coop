//! Apple sandbox backend (`apple-container` feature, macOS only).
//!
//! Runs each coop instance as a persistent `coop-sandbox` VM
//! (`macos/coop-sandbox`, built on `apple/containerization`): one VM per
//! instance, each on its own vmnet network, with no host mounts, socket
//! relays, published ports, or SSH-agent forwarding. coop reuses its SSH-based
//! guest operations over a pinned per-instance host key. Workspaces are
//! copied, never live-mounted.
//!
//! Images are built from a generated Dockerfile by the stock Apple
//! `container` CLI (`container build`) and imported into the runtime's
//! private store. Every path that boots a guest or hands one out first
//! qualifies the runtime and passes the isolation gate (`security`); cleanup
//! of owned resources works without either.

mod cli;
mod image;
mod protocol;
mod security;
mod ssh;
mod state;

use std::cell::OnceCell;
use std::collections::HashSet;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::backend::{
    BackendCapabilities, Capability, LocalEndpointRoute, LogMode, RunningInstance, SshTarget,
    StoppedInstance, VmBackend, boot_preflight,
};
use crate::config::{
    AppleContainerConfig, CoopConfig, GiB, ImageName, Instance, Mount, NetworkConfig, VmMemory,
};
use crate::setup::SetupOptions;
use cli::{Exec, MAX_JSON_OUTPUT, MAX_PUBKEY_OUTPUT, MAX_TEXT_OUTPUT, RealExec, Request};
use image::{BuildContext, BuildInputs, CommittedDisk, ImageManifest};
use protocol::{Inspect, SandboxStatus};
use security::{Expected, QualifiedRuntime, SecurityReady};
use state::{
    CreateStage, DestroyStage, Journal, JournalOp, MachineName, MachineSidecar, OperationId, Owner,
    Resources,
};

/// Stable diagnostic classes. The prefix of each message is the identifier
/// documented in `docs/backends.md`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AppleError {
    #[error("APPLE_RUNTIME_UNAVAILABLE: {0}")]
    RuntimeUnavailable(String),
    #[error("APPLE_RUNTIME_UNQUALIFIED: {0}")]
    RuntimeUnqualified(String),
    #[error("APPLE_NETWORK_ISOLATION: {0}")]
    NetworkIsolation(String),
    #[error("APPLE_HOST_EXPOSURE: {0}")]
    HostExposure(String),
    #[error("APPLE_IDENTITY_CONFLICT: {0}")]
    IdentityConflict(String),
    #[error("APPLE_HOST_KEY_CHANGED: {0}")]
    HostKeyChanged(String),
    #[error("APPLE_BOOT_TIMEOUT: {0}")]
    BootTimeout(String),
    #[error("APPLE_OPERATION_UNCERTAIN: {0}")]
    OperationUncertain(String),
}

/// Install locations searched for `coop-sandbox` when `[apple_container]
/// binary` is unset; the first is `scripts/build-coop-sandbox.sh`'s default.
fn default_runtimes() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        v.push(PathBuf::from(home).join(".local/opt/coop-sandbox/bin/coop-sandbox"));
    }
    v.push("/usr/local/bin/coop-sandbox".into());
    v.push("/opt/homebrew/bin/coop-sandbox".into());
    v
}

/// Install locations searched for the stock image builder.
const DEFAULT_BUILDERS: &[&str] = &["/usr/local/bin/container", "/opt/homebrew/bin/container"];

/// Where stock Apple `container` installs its default guest kernel.
fn default_kernel() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join("Library/Application Support/com.apple.container/kernels/default.kernel-arm64")
    })
}

/// Oldest macOS release `apple/containerization`'s vmnet networks support.
const MIN_MACOS_MAJOR: u32 = 26;

/// Name of the runtime state root under the backend's state directory.
const RUNTIME_DIR: &str = "runtime";

/// Settings from `[apple_container]`, plus the runtime state root.
#[derive(Debug, Clone)]
struct Settings {
    binary: Option<PathBuf>,
    builder: Option<PathBuf>,
    kernel: Option<PathBuf>,
    runtime_root: PathBuf,
    probe: Duration,
    operation: Duration,
    create: Duration,
    boot: Duration,
    stop: Duration,
    build: Duration,
}

impl Settings {
    fn new(cfg: &AppleContainerConfig, state_root: &Path) -> Self {
        Self {
            binary: cfg.binary.as_ref().map(|p| p.to_path_buf()),
            builder: cfg.builder.as_ref().map(|p| p.to_path_buf()),
            kernel: cfg.kernel.as_ref().map(|p| p.to_path_buf()),
            runtime_root: state_root.join(RUNTIME_DIR),
            probe: cfg.probe_timeout_seconds.duration(),
            operation: cfg.operation_timeout_seconds.duration(),
            create: cfg.create_timeout_seconds.duration(),
            boot: cfg.boot_timeout_seconds.duration(),
            stop: cfg.stop_timeout_seconds.duration(),
            build: cfg.build_timeout_seconds.duration(),
        }
    }
}

/// A resolved `coop-sandbox` plus what qualification found. Qualification
/// failure is kept (not raised) so cleanup of owned resources still works on
/// an unqualified runtime; every path that boots or hands out a guest calls
/// [`Runtime::qualified`] first.
struct Runtime {
    exec: Box<dyn Exec>,
    settings: Settings,
    /// Canonical runtime state root, passed as `--root` on every call. The
    /// runtime reports paths under it, and the gate compares them exactly.
    root: PathBuf,
    qualification: std::result::Result<QualifiedRuntime, AppleError>,
}

/// The stock `container` CLI, used only to build images.
struct Builder {
    exec: Box<dyn Exec>,
}

pub struct AppleContainerBackend {
    settings: Settings,
    runtime: OnceCell<Runtime>,
    builder: OnceCell<Builder>,
    /// Runtime executor substituted by tests, taken on first use.
    #[cfg(test)]
    injected: std::cell::RefCell<Option<Box<dyn Exec>>>,
}

impl AppleContainerBackend {
    /// Backend with default `[apple_container]` settings.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::for_config(&CoopConfig::default())
    }

    /// Backend honouring the configured binaries, state root, and deadlines.
    pub fn for_config(cfg: &CoopConfig) -> Self {
        Self {
            settings: Settings::new(&cfg.apple_container, &cfg.state_root()),
            runtime: OnceCell::new(),
            builder: OnceCell::new(),
            #[cfg(test)]
            injected: std::cell::RefCell::new(None),
        }
    }

    #[cfg(test)]
    fn with_exec(cfg: &CoopConfig, runtime: Box<dyn Exec>, builder: Box<dyn Exec>) -> Self {
        let be = Self::for_config(cfg);
        *be.injected.borrow_mut() = Some(runtime);
        let _ = be.builder.set(Builder { exec: builder });
        be
    }

    /// The runtime, resolved and probed once per process.
    fn runtime(&self) -> Result<&Runtime> {
        if let Some(rt) = self.runtime.get() {
            return Ok(rt);
        }
        #[cfg(test)]
        let injected = self.injected.borrow_mut().take();
        #[cfg(not(test))]
        let injected: Option<Box<dyn Exec>> = None;
        let exec = match injected {
            Some(exec) => exec,
            None => Box::new(RealExec::new(resolve_binary(
                self.settings.binary.as_deref(),
                &default_runtimes(),
                Tool::Runtime,
            )?)),
        };
        let mut rt = Runtime {
            exec,
            settings: self.settings.clone(),
            root: canonical_path(&self.settings.runtime_root),
            qualification: Err(AppleError::RuntimeUnavailable(String::new())),
        };
        rt.qualification = rt
            .probe_qualification()
            .map_err(|e| match e.downcast::<AppleError>() {
                Ok(apple) => apple,
                Err(other) => AppleError::RuntimeUnavailable(format!("{other:#}")),
            });
        Ok(self.runtime.get_or_init(|| rt))
    }

    /// Runtime that passed qualification.
    fn qualified_runtime(&self) -> Result<(&Runtime, &QualifiedRuntime)> {
        let rt = self.runtime()?;
        let q = rt.qualified()?;
        Ok((rt, q))
    }

    /// The stock image builder, with its service running.
    fn builder(&self) -> Result<&Builder> {
        if let Some(b) = self.builder.get() {
            return Ok(b);
        }
        let binary = resolve_binary(
            self.settings.builder.as_deref(),
            &DEFAULT_BUILDERS
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
            Tool::Builder,
        )?;
        Ok(self.builder.get_or_init(|| Builder {
            exec: Box::new(RealExec::new(binary)),
        }))
    }
}

impl std::fmt::Display for AppleContainerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("apple-container")
    }
}

// ── Preflight ─────────────────────────────────────────────────

/// The two binaries the backend runs.
#[derive(Debug, Clone, Copy)]
enum Tool {
    /// `coop-sandbox`.
    Runtime,
    /// Stock Apple `container`, for image builds.
    Builder,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::Runtime => "coop-sandbox",
            Self::Builder => "container",
        }
    }

    fn install_hint(self) -> &'static str {
        match self {
            Self::Runtime => {
                "Build it with scripts/build-coop-sandbox.sh, or set `[apple_container] binary`."
            }
            Self::Builder => {
                "Install Apple `container` 1.4.1 or later, or set `[apple_container] builder`."
            }
        }
    }
}

/// Resolve a binary to an absolute, host-owned executable. Only the user's
/// own config or fixed install paths are consulted — never `PATH` or anything
/// a project supplies — so a project cannot choose the runtime.
fn resolve_binary(configured: Option<&Path>, defaults: &[PathBuf], tool: Tool) -> Result<PathBuf> {
    let candidates: Vec<PathBuf> = match configured {
        Some(p) => vec![p.to_path_buf()],
        None => defaults.to_vec(),
    };
    let mut last_err = None;
    for candidate in candidates {
        match check_binary(&candidate) {
            Ok(resolved) => return Ok(resolved),
            Err(e) => last_err = Some(e),
        }
    }
    let detail = last_err.map(|e| format!(" ({e:#})")).unwrap_or_default();
    Err(AppleError::RuntimeUnavailable(format!(
        "no usable `{}` found{detail}. {}",
        tool.name(),
        tool.install_hint()
    ))
    .into())
}

fn check_binary(path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    let resolved = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;
    let meta = std::fs::metadata(&resolved)?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if !meta.is_file() || meta.mode() & 0o111 == 0 {
        bail!("{} is not an executable file", resolved.display());
    }
    if meta.uid() != 0 && meta.uid() != uid {
        bail!("{} is owned by another user", resolved.display());
    }
    if meta.mode() & 0o022 != 0 {
        bail!("{} is group- or world-writable", resolved.display());
    }
    // Whoever can rename entries in a directory on the path can swap the
    // binary, so every ancestor must be as trustworthy as the file.
    for dir in resolved.ancestors().skip(1) {
        let meta = std::fs::metadata(dir)
            .with_context(|| format!("Failed to inspect {}", dir.display()))?;
        if let Some(why) = untrusted_dir(meta.mode(), meta.uid(), meta.gid(), uid) {
            bail!(
                "{} is inside {}, which {why}",
                resolved.display(),
                dir.display()
            );
        }
    }
    Ok(resolved)
}

/// Group ids whose members can already act as root through `sudo` on
/// macOS (`wheel`, `admin`), so a directory writable by them grants nothing
/// root does not. Homebrew's prefix is `admin`-group-writable.
const ROOT_EQUIVALENT_GIDS: &[u32] = &[0, 80];

/// Why a directory on a binary's path lets someone other than root or `me`
/// replace entries in it, or `None` if it does not. A sticky directory only
/// lets each user rename their own entries.
fn untrusted_dir(mode: u32, owner: u32, group: u32, me: u32) -> Option<&'static str> {
    const STICKY: u32 = 0o1000;
    if owner != 0 && owner != me {
        return Some("is owned by another user");
    }
    if mode & STICKY != 0 {
        return None;
    }
    if mode & 0o002 != 0 {
        return Some("is world-writable");
    }
    if mode & 0o020 != 0 && !ROOT_EQUIVALENT_GIDS.contains(&group) {
        return Some("is group-writable");
    }
    None
}

/// `path` with its longest existing ancestor canonicalized, so it reads the
/// same before and after the missing tail is created (matching
/// `coop-sandbox`, which canonicalizes its root the same way).
fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical_path(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// Apple Silicon and a supported macOS release.
fn check_platform() -> Result<()> {
    if !cfg!(target_arch = "aarch64") {
        bail!(AppleError::RuntimeUnavailable(
            "the Apple sandbox backend requires an Apple Silicon Mac".into()
        ));
    }
    let out = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .context("Failed to run sw_vers")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let major = text
        .trim()
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .context("Unrecognised sw_vers output")?;
    if major < MIN_MACOS_MAJOR {
        bail!(AppleError::RuntimeUnavailable(format!(
            "the Apple sandbox backend requires macOS {MIN_MACOS_MAJOR} or later (found {})",
            text.trim()
        )));
    }
    Ok(())
}

// ── Runtime operations ────────────────────────────────────────

/// What a new sandbox clones.
enum Source<'a> {
    Image(&'a str),
    Disk(&'a MachineName),
}

impl Runtime {
    fn qualified(&self) -> Result<&QualifiedRuntime> {
        self.qualification.as_ref().map_err(|e| e.clone().into())
    }

    fn probe_qualification(&self) -> Result<QualifiedRuntime> {
        let json = self.text(["version"], self.settings.probe, MAX_TEXT_OUTPUT)?;
        security::qualify(&protocol::parse_version(&json)?)
    }

    /// `<subcommand...> --root <root> <rest...>`.
    fn args(&self, sub: &[&str], rest: &[&str]) -> Vec<String> {
        sub.iter()
            .map(|s| (*s).to_string())
            .chain(["--root".to_string(), self.root.display().to_string()])
            .chain(rest.iter().map(|s| (*s).to_string()))
            .collect()
    }

    /// Run a command that must succeed; return its stdout.
    fn text<I, S>(&self, args: I, timeout: Duration, max: usize) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.checked(&Request::new(args, timeout, max))
    }

    /// Run `req`, which must succeed; return its stdout.
    fn checked(&self, req: &Request) -> Result<String> {
        let out = self.exec.run(req)?;
        if !out.success() {
            bail!(
                "`coop-sandbox {}` failed: {}",
                req.describe(),
                out.stderr_summary()
            );
        }
        Ok(out.stdout_str()?.to_string())
    }

    fn expected<'a>(&'a self, sidecar: &'a MachineSidecar) -> Expected<'a> {
        Expected {
            sandbox: &sidecar.machine_id,
            owner: &sidecar.owner_id,
            runtime_root: &self.root,
            resources: sidecar.resources(),
        }
    }

    fn init(&self, kernel: &Path) -> Result<()> {
        state::ensure_private_dir(&self.root)?;
        let kernel = kernel.display().to_string();
        // The first init pulls the pinned init image; later ones are no-ops.
        self.text(
            self.args(&["init"], &["--kernel", &kernel]),
            self.settings.create,
            MAX_TEXT_OUTPUT,
        )?;
        Ok(())
    }

    fn inspect(&self, name: &MachineName) -> Result<Inspect> {
        let json = self.text(
            self.args(&["inspect"], &[name.as_str()]),
            self.settings.probe,
            MAX_JSON_OUTPUT,
        )?;
        protocol::parse_inspect(&json, name)
    }

    /// Whether the runtime lists `name`. A failed listing is an error, never
    /// "absent".
    fn exists(&self, name: &MachineName) -> Result<bool> {
        Ok(self.listed(name)?.is_some())
    }

    /// `name`'s status from the runtime's listing, or `None` if it is not
    /// listed. Unlike `inspect`, the listing never reads a staged disk
    /// update, so it works on a sandbox whose staged state is unreadable.
    fn listed(&self, name: &MachineName) -> Result<Option<SandboxStatus>> {
        let json = self.text(
            self.args(&["list"], &[]),
            self.settings.probe,
            MAX_JSON_OUTPUT,
        )?;
        Ok(protocol::parse_list(&json)?
            .into_iter()
            .find(|s| s.id == name.as_str())
            .map(|s| s.status))
    }

    fn create(
        &self,
        name: &MachineName,
        source: &Source<'_>,
        cpus: NonZeroU8,
        memory_mib: u32,
        disk: GiB,
        owner: &Owner,
    ) -> Result<()> {
        let req = Request::new(
            create_args(self, name, source, cpus, memory_mib, disk, owner),
            self.settings.create,
            MAX_JSON_OUTPUT,
        )
        .cancellable();
        self.checked(&req)?;
        Ok(())
    }

    /// Last lines of the sandbox's console log, for boot-failure errors: the
    /// only diagnostic available when SSH never came up. Best effort;
    /// control characters from the guest are replaced.
    fn boot_log_tail(&self, name: &MachineName) -> String {
        let req = Request::new(
            self.args(&["logs"], &[name.as_str(), "-n", "40"]),
            self.settings.probe,
            MAX_JSON_OUTPUT,
        );
        match self.checked(&req) {
            Ok(text) if !text.trim().is_empty() => {
                format!(
                    "\nLast console log lines:\n{}",
                    cli::sanitize_for_display(&text)
                )
            }
            _ => format!("\n(Console log unavailable; try `coop logs`.) [{name}]"),
        }
    }

    /// Boot `name` under launchd; returns once its owner answers.
    fn boot(&self, name: &MachineName) -> Result<()> {
        let wait = self.settings.boot.as_secs().to_string();
        let req = Request::new(
            self.args(&["start"], &[name.as_str(), "--wait-seconds", &wait]),
            self.settings.boot + Duration::from_secs(10),
            MAX_JSON_OUTPUT,
        )
        .cancellable();
        self.checked(&req).map_err(|e| {
            AppleError::BootTimeout(format!(
                "sandbox {name} failed to boot: {e:#}{}",
                self.boot_log_tail(name)
            ))
        })?;
        Ok(())
    }

    /// Wait until `name` runs with its effective configuration, then run the
    /// isolation gate.
    fn wait_ready(&self, expected: &Expected<'_>, deadline: Instant) -> Result<SecurityReady> {
        let name = expected.sandbox;
        loop {
            crate::signal::check_shutdown()?;
            let rec = self.inspect(name)?;
            match rec.status {
                SandboxStatus::Running if rec.effective.is_some() => {
                    return security::verify_effective(&rec, expected);
                }
                SandboxStatus::Stopped | SandboxStatus::Crashed => {
                    bail!(AppleError::BootTimeout(format!(
                        "sandbox {name} {} during boot{}",
                        rec.status.label(),
                        self.boot_log_tail(name)
                    )));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                bail!(AppleError::BootTimeout(format!(
                    "sandbox {name} did not report a running configuration in time{}",
                    self.boot_log_tail(name)
                )));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Boot `expected.sandbox` and read its host key, all by `deadline`. The
    /// three conditions stay distinct and ordered: the owner answers
    /// (`boot`), the effective configuration passes the isolation gate
    /// (`wait_ready`), and the key is read over the native channel from the
    /// boot the gate approved. Trusting the key and SSH readiness are the
    /// caller's next steps ([`pin_host_key`], [`wait_for_ssh`]). On error the
    /// sandbox may be running; the caller stops or deletes it.
    fn boot_validated(
        &self,
        expected: &Expected<'_>,
        deadline: Instant,
    ) -> Result<(SecurityReady, ssh::HostPublicKey)> {
        self.boot(expected.sandbox)?;
        let ready = self.wait_ready(expected, deadline)?;
        let key = self.read_host_key(&ready, deadline)?;
        Ok((ready, key))
    }

    /// Change a stopped sandbox's CPU/memory as operation `op`. With
    /// `expect`, the runtime refuses unless its last committed operation is
    /// still `expect`, checked under its own per-sandbox guard.
    fn set_resources(
        &self,
        name: &MachineName,
        target: Resources,
        op: &OperationId,
        expect: Option<&OperationId>,
    ) -> Result<()> {
        let cpus = target.cpus.to_string();
        let mem = (target.memory_bytes / MIB).to_string();
        let mut rest = vec![
            name.as_str(),
            "--cpus",
            cpus.as_str(),
            "--memory-mib",
            mem.as_str(),
            "--operation",
            op.as_str(),
        ];
        if let Some(expect) = expect {
            rest.extend(["--expect-operation", expect.as_str()]);
        }
        self.text(
            self.args(&["set"], &rest),
            self.settings.operation,
            MAX_JSON_OUTPUT,
        )?;
        Ok(())
    }

    /// The installed maintenance artifact, if any.
    fn maintenance(&self) -> Result<Option<protocol::MaintenanceArtifact>> {
        let json = self.text(
            self.args(&["maintenance", "inspect"], &[]),
            self.settings.probe,
            MAX_JSON_OUTPUT,
        )?;
        protocol::parse_maintenance(&json)
    }

    /// Run a fixed command as root in the guest over the native control
    /// channel (vsock). Arguments reach the guest as an argv, never a shell
    /// string.
    fn guest(
        &self,
        name: &MachineName,
        argv: &[&str],
        timeout: Duration,
        max: usize,
    ) -> Result<cli::Output> {
        let secs = timeout.as_secs().max(1).to_string();
        let mut rest = vec!["--timeout", secs.as_str(), name.as_str(), "--"];
        rest.extend_from_slice(argv);
        self.exec.run(&Request::new(
            self.args(&["exec"], &rest),
            timeout + Duration::from_secs(30),
            max,
        ))
    }

    /// Read the guest host public key over the native control channel,
    /// retrying until first-boot key generation finishes.
    fn read_host_key(
        &self,
        ready: &SecurityReady,
        deadline: Instant,
    ) -> Result<ssh::HostPublicKey> {
        let name = ready.sandbox();
        loop {
            crate::signal::check_shutdown()?;
            let out = self.guest(
                name,
                &["/bin/cat", "/etc/ssh/ssh_host_ed25519_key.pub"],
                self.settings.probe,
                MAX_PUBKEY_OUTPUT,
            )?;
            // A missing file and a file caught mid-write (empty or partial)
            // both mean key generation has not finished: poll again. Only a
            // key that still does not parse at the deadline is an error.
            let attempt = if out.success() {
                out.stdout_str()
                    .map(String::from)
                    .and_then(|text| ssh::HostPublicKey::parse(&text))
            } else {
                Err(anyhow::anyhow!("{}", out.stderr_summary()))
            };
            match attempt {
                Ok(key) => {
                    // The key must come from the boot the gate approved.
                    let rec = self.inspect(name)?;
                    if rec.live.as_ref().map(|l| l.pid) != Some(ready.owner_pid()) {
                        bail!(AppleError::IdentityConflict(format!(
                            "sandbox {name} restarted while its host key was read"
                        )));
                    }
                    return Ok(key);
                }
                Err(e) if Instant::now() >= deadline => {
                    bail!(AppleError::BootTimeout(format!(
                        "sandbox {name} did not produce a valid SSH host key in time: {e:#}"
                    )));
                }
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Request a clean stop and confirm it. A stop that cannot be confirmed
    /// leaves the sandbox in an unknown state; nothing may mutate its disk.
    fn stop_and_confirm(&self, name: &MachineName) -> Result<()> {
        let secs = self.settings.stop.as_secs().to_string();
        let req = Request::new(
            self.args(&["stop"], &[name.as_str(), "--timeout-seconds", &secs]),
            self.settings.stop + Duration::from_secs(120),
            MAX_TEXT_OUTPUT,
        );
        let out = self.exec.run(&req)?;
        let rec = self.inspect(name)?;
        if rec.status != SandboxStatus::Stopped {
            bail!(AppleError::OperationUncertain(format!(
                "sandbox {name} did not confirm it stopped (now {}; {}); leaving it untouched",
                rec.status.label(),
                out.stderr_summary()
            )));
        }
        Ok(())
    }

    fn delete(&self, name: &MachineName, owner: &Owner) -> Result<()> {
        self.text(
            self.args(&["delete"], &[name.as_str(), "--owner", owner.id.as_str()]),
            self.settings.operation,
            MAX_TEXT_OUTPUT,
        )?;
        if self.exists(name)? {
            bail!(AppleError::OperationUncertain(format!(
                "sandbox {name} still exists after delete"
            )));
        }
        Ok(())
    }

    fn images(&self) -> Result<Vec<protocol::ImageEntry>> {
        let json = self.text(
            self.args(&["image", "list"], &[]),
            self.settings.probe,
            MAX_JSON_OUTPUT,
        )?;
        protocol::parse_images(&json)
    }

    fn disks(&self) -> Result<Vec<protocol::DiskEntry>> {
        let json = self.text(
            self.args(&["disk", "list"], &[]),
            self.settings.probe,
            MAX_JSON_OUTPUT,
        )?;
        protocol::parse_disks(&json)
    }

    /// The manifest's image (or committed disk) must still be in the store
    /// with the verified content.
    fn verify_image(&self, manifest: &ImageManifest) -> Result<()> {
        if let Some(disk) = &manifest.disk {
            if !self.disks()?.iter().any(|d| d.name == disk.name.as_str()) {
                bail!(AppleError::IdentityConflict(format!(
                    "committed disk {} for image {} is missing from the runtime",
                    disk.name, manifest.image_ref
                )));
            }
            return Ok(());
        }
        let images = self.images()?;
        let Some(found) = images.iter().find(|i| i.reference == manifest.image_ref) else {
            bail!(AppleError::IdentityConflict(format!(
                "image {} is missing from the runtime; run `coop setup --rebuild`",
                manifest.image_ref
            )));
        };
        if found.digest != manifest.digest {
            bail!(AppleError::IdentityConflict(format!(
                "image {} now resolves to {}, but the template was verified as {}; \
                 run `coop setup --rebuild`",
                manifest.image_ref, found.digest, manifest.digest
            )));
        }
        Ok(())
    }

    /// Poll until every unit in `services` is active. Units start after the
    /// SSH host key appears (docker well after), so "activating" is retried;
    /// a unit still inactive at `deadline` fails verification.
    fn wait_for_services(
        &self,
        name: &MachineName,
        services: &[&str],
        deadline: Instant,
    ) -> Result<()> {
        loop {
            let mut args = vec!["/usr/bin/systemctl", "is-active", "--quiet"];
            args.extend_from_slice(services);
            if self.run_root_ok(name, &args) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let mut states = vec!["/usr/bin/systemctl", "is-active"];
                states.extend_from_slice(services);
                let detail = self
                    .guest(name, &states, self.settings.probe, MAX_TEXT_OUTPUT)
                    .map(|o| cli::sanitize_for_display(&String::from_utf8_lossy(&o.stdout)))
                    .unwrap_or_default();
                bail!(
                    "Image verification failed: services {} not active in time ({})",
                    services.join(", "),
                    detail.replace('\n', ", ")
                );
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Which of `paths` are not executable in the guest, from one exec.
    /// Paths reach the script as positional parameters, never as script text.
    fn missing_executables(&self, name: &MachineName, paths: &[String]) -> Result<Vec<String>> {
        let mut command = vec![
            "/bin/sh",
            "-c",
            r#"for p; do test -x "$p" || printf '%s\n' "$p"; done"#,
            "sh",
        ];
        command.extend(paths.iter().map(String::as_str));
        let out = self.guest(name, &command, self.settings.operation, MAX_TEXT_OUTPUT)?;
        if !out.success() {
            bail!("guest check failed: {}", out.stderr_summary());
        }
        Ok(out.stdout_str()?.lines().map(String::from).collect())
    }

    /// Run a fixed root command in the guest; `true` on exit 0.
    fn run_root_ok(&self, name: &MachineName, command: &[&str]) -> bool {
        self.guest(name, command, self.settings.operation, MAX_TEXT_OUTPUT)
            .is_ok_and(|o| o.success())
    }

    fn delete_image_best_effort(&self, image_ref: &str) {
        if let Err(e) = self.text(
            self.args(&["image", "delete"], &[image_ref]),
            self.settings.operation,
            MAX_TEXT_OUTPUT,
        ) {
            tracing::warn!("Failed to delete image {image_ref}: {e:#}");
        }
    }

    fn delete_disk_best_effort(&self, disk: &MachineName) {
        if let Err(e) = self.text(
            self.args(&["disk", "delete"], &[disk.as_str()]),
            self.settings.operation,
            MAX_TEXT_OUTPUT,
        ) {
            tracing::warn!("Failed to delete committed disk {disk}: {e:#}");
        }
    }

    /// Remove uncommitted creates, interrupted deletes, and scratch files.
    fn reconcile_best_effort(&self) {
        if let Err(e) = self.text(
            self.args(&["reconcile"], &[]),
            self.settings.operation,
            MAX_JSON_OUTPUT,
        ) {
            tracing::warn!("Failed to reconcile the sandbox runtime: {e:#}");
        }
    }

    /// Best-effort stop after a failed boot, keeping the disk for diagnosis.
    fn stop_after_failure(&self, name: &MachineName) {
        if let Err(e) = self.stop_and_confirm(name) {
            tracing::warn!("Failed to stop sandbox {name} after a failed start: {e:#}");
        }
    }
}

impl Builder {
    fn text(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let req = Request::new(args.iter().copied(), timeout, MAX_TEXT_OUTPUT);
        let out = self.exec.run(&req)?;
        if !out.success() {
            bail!(
                "`container {}` failed: {}",
                req.describe(),
                out.stderr_summary()
            );
        }
        Ok(out.stdout_str()?.to_string())
    }

    /// The builder needs its service; coop never starts or stops it.
    fn require_service(&self, timeout: Duration) -> Result<()> {
        self.text(&["system", "status"], timeout)
            .map(|_| ())
            .map_err(|e| {
                AppleError::RuntimeUnavailable(format!(
                    "the Apple `container` service (used to build images) is not running ({e:#}). \
                 Start it with `container system start`, then retry."
                ))
                .into()
            })
    }
}

/// How a boot treats the guest's SSH host key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostKeyTrust {
    /// First boot of a new instance: pin the key it generated.
    Enroll,
    /// An existing instance: the key must match its pin.
    RequirePin,
    /// coop itself replaced the disk (`restore`, correlated by its
    /// operation id): pin the new key.
    ReenrollAfterRestore,
}

fn pin_host_key(
    inst: &Instance,
    machine: &MachineName,
    key: &ssh::HostPublicKey,
    trust: HostKeyTrust,
) -> Result<()> {
    match trust {
        HostKeyTrust::Enroll => ssh::enroll(inst, machine, key),
        HostKeyTrust::RequirePin => ssh::check_pin(inst, machine, key),
        HostKeyTrust::ReenrollAfterRestore => {
            ssh::reenroll_after_disk_replacement(inst, machine, key)
        }
    }
}

/// Wait until pinned SSH accepts connections, within what is left of
/// `deadline` (at least 5 s).
fn wait_for_ssh(
    cfg: &CoopConfig,
    inst: &Instance,
    ready: &SecurityReady,
    user: &crate::guest::GuestUser,
    deadline: Instant,
) -> Result<()> {
    ssh::pinned_target(cfg, inst, ready.sandbox(), ready.ip(), user)?
        .wait_until_ready(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(5)),
        )
        .context("Guest booted but SSH is not accepting connections")
}

/// Argument vector for `coop-sandbox create`. There is no argument for a
/// host mount, socket, port, network, or agent: the runtime cannot express them.
fn create_args(
    rt: &Runtime,
    name: &MachineName,
    source: &Source<'_>,
    cpus: NonZeroU8,
    memory_mib: u32,
    disk: GiB,
    owner: &Owner,
) -> Vec<String> {
    let (flag, value) = match source {
        Source::Image(r) => ("--image", (*r).to_string()),
        Source::Disk(d) => ("--from-disk", d.to_string()),
    };
    let cpus = cpus.to_string();
    let mem = memory_mib.to_string();
    let disk = disk.to_string();
    rt.args(
        &["create"],
        &[
            name.as_str(),
            flag,
            &value,
            "--cpus",
            &cpus,
            "--memory-mib",
            &mem,
            "--disk-gib",
            &disk,
            "--owner",
            owner.id.as_str(),
        ],
    )
}

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn mib_to_bytes(mib: u32) -> u64 {
    u64::from(mib) * MIB
}

/// Whole GiB covering `bytes` (committed disks are always whole GiB).
fn covering_gib(bytes: u64) -> Result<GiB> {
    let gib = u32::try_from(bytes.div_ceil(GIB)).context("disk size")?;
    GiB::new(gib).context("disk size is zero")
}

// ── Lifecycle ─────────────────────────────────────────────────

impl AppleContainerBackend {
    #[expect(clippy::too_many_arguments, reason = "one create, all inputs explicit")]
    fn provision_sandbox(
        cfg: &CoopConfig,
        rt: &Runtime,
        q: &QualifiedRuntime,
        inst: &Instance,
        owner: &Owner,
        manifest: &ImageManifest,
        disk: GiB,
        journal: &mut Journal,
    ) -> Result<()> {
        let machine = journal.machine_id.clone();
        let cpus = cfg.vm.vcpu_count;
        let memory_mib = cfg.vm.mem_size_mib.get().as_u32();
        let source = match &manifest.disk {
            Some(d) => Source::Disk(&d.name),
            None => Source::Image(&manifest.image_ref),
        };

        journal.advance(
            inst,
            JournalOp::Create {
                stage: CreateStage::CreatingMachine,
            },
        )?;
        rt.create(&machine, &source, cpus, memory_mib, disk, owner)?;
        journal.advance(
            inst,
            JournalOp::Create {
                stage: CreateStage::MachineCreated,
            },
        )?;

        let mut sidecar = MachineSidecar {
            schema_version: state::SCHEMA_VERSION,
            backend: state::BACKEND_TAG.into(),
            owner_id: owner.id.clone(),
            machine_id: machine.clone(),
            image_ref: manifest.image_ref.clone(),
            image_digest: manifest.digest.clone(),
            image_manifest_id: manifest.manifest_id.clone(),
            guest_user: manifest.guest_user.clone(),
            requested_cpus: u32::from(cpus.get()),
            requested_memory_bytes: mib_to_bytes(memory_mib),
            host_key_fingerprint: String::new(),
            last_observed_owner_pid: None,
            last_observed_ip: None,
            reenroll_host_key: false,
            created_at: crate::setup::utc_timestamp(),
            runtime_identity: q.identity.clone(),
        };
        security::verify_record(&rt.inspect(&machine)?, &rt.expected(&sidecar))?;

        let deadline = Instant::now() + rt.settings.boot;
        let (ready, key) = rt.boot_validated(&rt.expected(&sidecar), deadline)?;
        pin_host_key(inst, &machine, &key, HostKeyTrust::Enroll)?;
        wait_for_ssh(cfg, inst, &ready, &manifest.guest_user, deadline)?;

        sidecar.host_key_fingerprint = key.fingerprint();
        sidecar.last_observed_owner_pid = Some(ready.owner_pid());
        sidecar.last_observed_ip = Some(ready.ip());
        sidecar.save(inst)
    }

    /// Load and ownership-check an instance's record.
    fn owned_sidecar(cfg: &CoopConfig, inst: &Instance) -> Result<MachineSidecar> {
        let owner = Owner::load(cfg)?;
        if let Some(journal) = Journal::try_load(inst)? {
            bail!(AppleError::OperationUncertain(format!(
                "instance '{}' has an unfinished {}; {}",
                inst.name,
                journal.op.describe(),
                journal.op.recovery_hint(&inst.name)
            )));
        }
        let sidecar = MachineSidecar::load(inst)?;
        sidecar.check_owner(&owner)?;
        Ok(sidecar)
    }

    /// Finish an interrupted resource change or restore once the sandbox is
    /// confirmed stopped: the runtime's record is authoritative, and its last
    /// committed operation says whether coop's own change applied.
    /// Idempotent: it only inspects the runtime, never changes it. Other journaled
    /// operations are left for `destroy`. Caller holds the lock.
    fn recover_journal(rt: &Runtime, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        let Some(journal) = Journal::try_load(inst)? else {
            return Ok(());
        };
        if !matches!(
            journal.op,
            JournalOp::SetResources { .. } | JournalOp::RestoreDisk { .. }
        ) {
            return Ok(());
        }
        let owner = Owner::load(cfg)?;
        let mut sidecar = MachineSidecar::load(inst)?;
        sidecar.check_owner(&owner)?;
        if journal.machine_id != sidecar.machine_id {
            bail!(AppleError::IdentityConflict(format!(
                "journal names {}, but the instance records {}",
                journal.machine_id, sidecar.machine_id
            )));
        }
        let rec = rt.inspect(&sidecar.machine_id)?;
        if rec.status != SandboxStatus::Stopped {
            bail!(AppleError::OperationUncertain(format!(
                "an interrupted {} of '{}' cannot be reconciled while the sandbox is {}",
                journal.op.describe(),
                inst.name,
                rec.status.label()
            )));
        }
        match &journal.op {
            JournalOp::SetResources { operation, .. } => {
                tracing::warn!(
                    "Reconciling an interrupted resource change of '{}' ({}): runtime reports {}",
                    inst.name,
                    resource_change_outcome(operation, &rec.record),
                    rec.record.resources()
                );
                sidecar.requested_cpus = rec.record.cpus;
                sidecar.requested_memory_bytes = rec.record.memory_bytes;
            }
            JournalOp::RestoreDisk {
                operation,
                prior_generation,
            } => {
                // Only coop's own restore, identified by its operation id,
                // authorizes a new host key.
                let applied = rec.record.disk_generation > *prior_generation
                    && rec.record.committed(operation);
                tracing::warn!(
                    "Reconciling an interrupted restore of '{}': {}",
                    inst.name,
                    if applied {
                        "the disk was replaced"
                    } else {
                        "the disk was not replaced by this restore"
                    }
                );
                if applied {
                    sidecar.image_ref.clone_from(&rec.record.image_reference);
                    sidecar.image_digest.clone_from(&rec.record.image_digest);
                    sidecar.reenroll_host_key = true;
                }
            }
            JournalOp::Create { .. } | JournalOp::Destroy { .. } => {}
        }
        sidecar.save(inst)?;
        Journal::complete(inst)
    }

    fn start_owned(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        let (rt, q) = self.qualified_runtime()?;
        let _lock = state::lock_instance(inst)?;
        Self::recover_journal(rt, cfg, inst)?;
        let mut sidecar = Self::owned_sidecar(cfg, inst)?;
        let machine = sidecar.machine_id.clone();
        let rec = rt.inspect(&machine)?;
        match rec.status {
            // A crashed owner left no VM; `coop-sandbox start` clears it.
            SandboxStatus::Stopped | SandboxStatus::Crashed => {}
            SandboxStatus::Running => bail!("Instance '{}' is already running", inst.name),
            SandboxStatus::Booting => bail!(AppleError::OperationUncertain(format!(
                "sandbox {machine} is booting; wait for it to settle"
            ))),
        }
        security::verify_record(&rec, &rt.expected(&sidecar))?;

        let deadline = Instant::now() + rt.settings.boot;
        let trust = if sidecar.reenroll_host_key {
            HostKeyTrust::ReenrollAfterRestore
        } else {
            HostKeyTrust::RequirePin
        };
        let booted = (|| -> Result<SecurityReady> {
            // Inside the guard: a boot that errors or times out may still
            // have started the sandbox, and it must not be left running.
            let (ready, key) = rt.boot_validated(&rt.expected(&sidecar), deadline)?;
            pin_host_key(inst, &machine, &key, trust)?;
            if trust == HostKeyTrust::ReenrollAfterRestore {
                // Close the window at once: a later start, even after this
                // one fails, must enforce the new pin.
                tracing::info!(
                    "Pinned the new host key of '{}' after its disk was restored ({})",
                    inst.name,
                    key.fingerprint()
                );
                sidecar.reenroll_host_key = false;
                sidecar.host_key_fingerprint = key.fingerprint();
                sidecar.save(inst)?;
            }
            wait_for_ssh(cfg, inst, &ready, &sidecar.guest_user, deadline)?;
            Ok(ready)
        })();
        let ready = match booted {
            Ok(ready) => ready,
            Err(e) => {
                rt.stop_after_failure(&machine);
                return Err(e);
            }
        };
        sidecar.last_observed_owner_pid = Some(ready.owner_pid());
        sidecar.last_observed_ip = Some(ready.ip());
        sidecar.runtime_identity.clone_from(&q.identity);
        sidecar.save(inst)
    }

    /// Qualify, gate, and build the pinned target from an inspection the
    /// caller already made.
    fn target_for(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        sidecar: &MachineSidecar,
        rec: &Inspect,
    ) -> Result<SshTarget> {
        let (rt, _) = self.qualified_runtime()?;
        let ready = security::verify_effective(rec, &rt.expected(sidecar))?;
        ssh::pinned_target(cfg, inst, ready.sandbox(), ready.ip(), &sidecar.guest_user)
    }

    /// Install the disk maintenance image unless the runtime already has
    /// this build's version. It is built from its own small recipe
    /// ([`image::MAINTENANCE_VERSION`]) with the stock builder, unpacked by
    /// the runtime outside its image store, then dropped from the store.
    fn ensure_maintenance(
        &self,
        cfg: &CoopConfig,
        rt: &Runtime,
        owner: &Owner,
        opts: &SetupOptions,
    ) -> Result<()> {
        if rt
            .maintenance()?
            .is_some_and(|m| m.version == image::MAINTENANCE_VERSION)
        {
            return Ok(());
        }
        let builder = self.builder()?;
        builder.require_service(rt.settings.probe)?;
        let reference = image::maintenance_ref(owner, &crate::fs_util::random_hex(4)?);
        let context = image::maintenance_context()?;
        let log = cfg.state_root().join("maintenance-build.log");
        crate::fs_util::atomic_write_with_mode(&log, "", 0o600)?;
        tracing::info!(
            "Building the disk maintenance image {reference} (log: {})",
            log.display()
        );
        let installed = build_into_runtime(
            rt,
            builder,
            &reference,
            context.path(),
            &log,
            opts.builder_timeout.unwrap_or(rt.settings.build),
        )
        .and_then(|_| {
            rt.text(
                rt.args(
                    &["maintenance", "install"],
                    &[
                        "--image",
                        &reference,
                        "--version",
                        image::MAINTENANCE_VERSION,
                    ],
                ),
                rt.settings.create,
                MAX_JSON_OUTPUT,
            )
        })
        .and_then(|json| protocol::parse_maintenance(&json));
        // The runtime keeps its own unpacked copy; the store entry is not
        // used again either way.
        rt.delete_image_best_effort(&reference);
        match installed? {
            Some(m) if m.version == image::MAINTENANCE_VERSION && m.reference == reference => {
                Ok(())
            }
            other => bail!(AppleError::RuntimeUnqualified(format!(
                "installing maintenance image {reference} reported {other:?}"
            ))),
        }
    }

    /// Create the runtime state root with the pinned kernel and init image.
    fn init_runtime(&self, rt: &Runtime) -> Result<()> {
        let kernel = self
            .settings
            .kernel
            .clone()
            .or_else(default_kernel)
            .context("no kernel path: set `[apple_container] kernel`")?;
        rt.init(&kernel).map_err(|e| {
            AppleError::RuntimeUnavailable(format!(
                "failed to initialize the sandbox runtime with kernel {}: {e:#}. The kernel is \
                 installed by Apple `container` (`container system start`), or set \
                 `[apple_container] kernel` to a validated kernel.",
                kernel.display()
            ))
            .into()
        })
    }

    /// Inspect a sandbox that must be stopped for a disk or resource change.
    fn require_stopped(rt: &Runtime, inst: &Instance, machine: &MachineName) -> Result<Inspect> {
        let rec = rt.inspect(machine)?;
        if rec.status != SandboxStatus::Stopped {
            bail!(
                "Instance '{}' is not stopped (sandbox is {})",
                inst.name,
                rec.status.label()
            );
        }
        Ok(rec)
    }

    /// Change a stopped instance's CPU/memory to `target` under the instance
    /// lock, journaled first; used for both a forward change and its
    /// rollback. With `undo`, refuses unless the runtime's last committed
    /// operation is still `undo.operation` with `undo.applied` resources
    /// (checked here and again by the runtime), so a rollback never
    /// overwrites a newer change. An unconfirmed outcome keeps the journal
    /// for the next start and is reported as uncertain.
    fn update_resources(
        rt: &Runtime,
        cfg: &CoopConfig,
        inst: &Instance,
        owner: &Owner,
        target: impl FnOnce(Resources) -> Resources,
        undo: Option<&ResourceUpdate>,
    ) -> Result<ResourceUpdate> {
        let _lock = state::lock_instance(inst)?;
        Self::recover_journal(rt, cfg, inst)?;
        let mut sidecar = Self::owned_sidecar(cfg, inst)?;
        let machine = sidecar.machine_id.clone();
        let rec = Self::require_stopped(rt, inst, &machine)?;
        let prior = rec.record.resources();
        if let Some(undo) = undo
            && !(rec.record.committed(&undo.operation)
                && prior == undo.applied
                && sidecar.resources() == undo.applied)
        {
            bail!(AppleError::OperationUncertain(format!(
                "sandbox {machine} changed after the update to {} (now {prior}, last operation \
                 {}); not rolling back over the newer change",
                undo.applied,
                rec.record.last_operation_label()
            )));
        }
        let target = target(prior);
        let operation = OperationId::generate()?;
        let journaled = JournalOp::SetResources {
            operation: operation.clone(),
            prior,
        };
        Journal::begin(inst, owner, journaled.clone(), machine.clone())?;
        rt.set_resources(&machine, target, &operation, undo.map(|u| &u.operation))?;
        let after = rt.inspect(&machine)?;
        if !after.record.committed(&operation) || after.record.resources() != target {
            bail!(AppleError::OperationUncertain(format!(
                "sandbox {machine} reports {} (last operation {}) after the update to {target}; {}",
                after.record.resources(),
                after.record.last_operation_label(),
                journaled.recovery_hint(&inst.name)
            )));
        }
        sidecar.requested_cpus = target.cpus;
        sidecar.requested_memory_bytes = target.memory_bytes;
        sidecar.save(inst)?;
        Journal::complete(inst)?;
        Ok(ResourceUpdate {
            operation,
            prior,
            applied: target,
        })
    }
}

/// How an interrupted resource change ended, for the reconcile warning only:
/// the sidecar takes the runtime's values either way.
fn resource_change_outcome(
    operation: &OperationId,
    record: &protocol::SandboxRecord,
) -> &'static str {
    if record.committed(operation) {
        "the change applied"
    } else {
        "the change did not apply"
    }
}

/// A resource change the runtime committed.
struct ResourceUpdate {
    operation: OperationId,
    prior: Resources,
    applied: Resources,
}

impl VmBackend for AppleContainerBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::new(&[
            Capability::DiskResize,
            Capability::DiskSnapshots,
            Capability::MachineResources,
        ])
    }

    fn setup(&self, cfg: &CoopConfig, opts: &SetupOptions) -> Result<()> {
        boot_preflight(cfg)?;
        check_platform()?;
        let (rt, q) = self.qualified_runtime()?;
        tracing::info!("Sandbox runtime: {}", q.identity);
        let owner = Owner::load_or_init(cfg)?;
        ensure_ssh_key(cfg)?;
        self.init_runtime(rt)?;
        self.ensure_maintenance(cfg, rt, &owner, opts)?;

        let pubkey_path = cfg.ssh_key_path().with_extension("pub");
        let pubkey = std::fs::read_to_string(&pubkey_path)
            .with_context(|| format!("Failed to read {}", pubkey_path.display()))?;
        let pubkey = pubkey.trim();
        let pubkey_fingerprint = ssh::ed25519_fingerprint(pubkey).map_err(|e| {
            anyhow::anyhow!(
                "VM access key {} {e}; delete it and rerun `coop setup` to regenerate it",
                pubkey_path.display()
            )
        })?;
        let ctx = BuildContext::render(&BuildInputs {
            pubkey,
            profiles: &opts.profiles,
            oci_features: &opts.oci_features,
            guest_user: &opts.guest_user,
        });
        let manifest_id = ctx.manifest_id(&opts.guest_user, &pubkey_fingerprint);
        let image = &opts.image;

        // An unreadable manifest must not block the rebuild that replaces it.
        let previous = ImageManifest::load_lenient(cfg, image);
        if !opts.rebuild
            && let Some(existing) = &previous
            && existing.manifest_id == manifest_id
            && rt.verify_image(existing).is_ok()
        {
            tracing::info!("Image '{image}' is up to date ({})", existing.image_ref);
            return Ok(());
        }

        let builder = self.builder()?;
        builder.require_service(rt.settings.probe)?;
        let image_ref = image::image_ref(&owner, &manifest_id, &crate::fs_util::random_hex(4)?);
        state::ensure_private_dir(&cfg.image_dir(image))?;
        let log = cfg.image_dir(image).join("build.log");
        crate::fs_util::atomic_write_with_mode(&log, "", 0o600)?;
        let context = ctx.materialize()?;
        tracing::info!(
            "Building image '{image}' as {image_ref} (log: {})",
            log.display()
        );
        let built = build_import_and_verify(
            rt,
            builder,
            cfg,
            &owner,
            &image_ref,
            &context,
            &log,
            // `coop setup --builder-timeout` overrides the configured deadline.
            opts.builder_timeout.unwrap_or(rt.settings.build),
            &opts.guest_user,
        );
        let digest = match built {
            Ok(digest) => digest,
            Err(e) => {
                // The new tag is unpublished; the previous manifest and its
                // image are untouched.
                rt.delete_image_best_effort(&image_ref);
                return Err(e);
            }
        };

        ImageManifest {
            schema_version: state::SCHEMA_VERSION,
            backend: state::BACKEND_TAG.into(),
            image_ref: image_ref.clone(),
            digest,
            disk: None,
            manifest_id: manifest_id.clone(),
            base_image: image::BASE_IMAGE.into(),
            platform: image::PLATFORM.into(),
            guest_user: opts.guest_user.clone(),
            created: crate::setup::utc_timestamp(),
        }
        .save(cfg, image)?;
        save_template_config(cfg, opts, &manifest_id)?;
        if let Some(old) = previous {
            release_manifest(rt, cfg, &owner, &old, None);
        }
        tracing::info!("Setup complete. Run `coop up` in a project directory to launch a VM.");
        Ok(())
    }

    fn create_and_start(
        &self,
        cfg: &CoopConfig,
        inst: &Instance,
        disk_gib: Option<GiB>,
        _mounts: &[Mount],
    ) -> Result<()> {
        boot_preflight(cfg)?;
        let (rt, q) = self.qualified_runtime()?;
        let owner = Owner::load(cfg)?;
        let manifest = ImageManifest::load(cfg, &inst.image)?;
        rt.verify_image(&manifest)?;
        let disk = match (disk_gib, &manifest.disk) {
            (Some(d), _) => d,
            (None, Some(committed)) => covering_gib(committed.bytes)?,
            (None, None) => cfg.vm.template_size_gib,
        };

        let _lock = state::lock_instance(inst)?;
        if MachineSidecar::try_load(inst)?.is_some() || Journal::try_load(inst)?.is_some() {
            bail!(AppleError::OperationUncertain(format!(
                "instance '{}' already has sandbox state; destroy it before recreating",
                inst.name
            )));
        }
        let machine = MachineName::generate(&owner.id)?;
        if rt.exists(&machine)? {
            bail!(AppleError::IdentityConflict(format!(
                "generated name {machine} is already in use; retry"
            )));
        }
        let mut journal = Journal::begin(
            inst,
            &owner,
            JournalOp::Create {
                stage: CreateStage::Reserved,
            },
            machine.clone(),
        )?;
        if let Err(e) =
            Self::provision_sandbox(cfg, rt, q, inst, &owner, &manifest, disk, &mut journal)
        {
            if matches!(journal.op, JournalOp::Create { stage } if stage >= CreateStage::CreatingMachine)
            {
                rt.stop_after_failure(&machine);
            }
            return Err(e);
        }
        Journal::complete(inst)
    }

    fn start_existing(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        boot_preflight(cfg)?;
        self.start_owned(cfg, inst)
    }

    fn stop(&self, cfg: &CoopConfig, running: RunningInstance) -> Result<()> {
        let (inst, _target) = running.into_parts();
        let rt = self.runtime()?;
        let _lock = state::lock_instance(&inst)?;
        let sidecar = Self::owned_sidecar(cfg, &inst)?;
        rt.stop_and_confirm(&sidecar.machine_id)
    }

    /// Stop through the runtime's control plane alone. Needs neither a
    /// qualified runtime nor a passing gate nor SSH, so a sandbox that can
    /// no longer be reached safely can still be stopped (its disk is kept).
    /// Ownership is still required; an unfinished journal does not block it.
    fn stop_unproven(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        let owner = Owner::load(cfg)?;
        let sidecar = MachineSidecar::load(inst)?;
        sidecar.check_owner(&owner)?;
        let rt = self.runtime()?;
        let _lock = state::lock_instance(inst)?;
        let rec = rt.inspect(&sidecar.machine_id)?;
        if rec.status != SandboxStatus::Stopped {
            rt.stop_and_confirm(&sidecar.machine_id)?;
        }
        tracing::info!("Instance '{}' stopped", inst.name);
        Ok(())
    }

    fn destroy_instance(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        if !inst.dir.exists() {
            return Ok(());
        }
        // Held until the directory (lock file included) is gone, so no other
        // mutation can change the records this reads.
        let _lock = state::lock_instance(inst)?;
        let sidecar = MachineSidecar::try_load(inst)?;
        let journal = Journal::try_load(inst)?;
        let ids = match (&sidecar, &journal) {
            (_, Some(j)) => Some((j.owner_id.clone(), j.machine_id.clone())),
            (Some(s), None) => Some((s.owner_id.clone(), s.machine_id.clone())),
            (None, None) => None,
        };
        if let Some((owner_id, machine)) = ids {
            let owner = Owner::load(cfg)?;
            if owner_id != owner.id || !machine.belongs_to(&owner.id) {
                bail!(AppleError::IdentityConflict(format!(
                    "instance '{}' records sandbox {machine}, which this installation does not own; \
                     leaving it untouched",
                    inst.name
                )));
            }
            let rt = self.runtime()?;
            // The listing, not `inspect`: the runtime refuses to inspect a
            // sandbox whose staged disk update is unreadable, and destroying
            // one must still work.
            if let Some(status) = rt.listed(&machine)? {
                if status != SandboxStatus::Stopped {
                    rt.stop_and_confirm(&machine)?;
                }
                let mut j = match journal {
                    Some(j) => j,
                    None => Journal::begin(
                        inst,
                        &owner,
                        JournalOp::Destroy {
                            stage: DestroyStage::Reserved,
                        },
                        machine.clone(),
                    )?,
                };
                j.advance(
                    inst,
                    JournalOp::Destroy {
                        stage: DestroyStage::DeletingMachine,
                    },
                )?;
                rt.delete(&machine, &owner)?;
                j.advance(
                    inst,
                    JournalOp::Destroy {
                        stage: DestroyStage::MachineDeleted,
                    },
                )?;
            }
            // A create or delete interrupted inside the runtime leaves an
            // unlisted directory there; reconcile removes it.
            rt.reconcile_best_effort();
        }
        remove_instance_dir(inst)
    }

    fn destroy_shared(&self, cfg: &CoopConfig) {
        let Ok(images) = cfg.list_images() else {
            return;
        };
        for info in images {
            if let Err(e) = self.destroy_image(cfg, &info.name) {
                tracing::warn!("Failed to remove image '{}': {e:#}", info.name);
            }
        }
    }

    fn destroy_image(&self, cfg: &CoopConfig, image: &ImageName) -> Result<()> {
        let dir = cfg.image_dir(image);
        if !dir.exists() {
            bail!("Image '{image}' does not exist");
        }
        // Deleting the runtime content is best effort: an unreadable
        // manifest, missing owner record, or unavailable runtime must not
        // leave the image name stuck. Instances never depend on it: each has
        // its own disk and captured environment.
        if let Some(manifest) = ImageManifest::load_lenient(cfg, image)
            && let Ok(owner) = Owner::load(cfg)
        {
            match self.runtime() {
                Ok(rt) => release_manifest(rt, cfg, &owner, &manifest, Some(image)),
                Err(e) => tracing::warn!(
                    "Could not delete image {} from the runtime: {e:#}",
                    manifest.image_ref
                ),
            }
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("Failed to remove image dir {}", dir.display()))?;
        tracing::info!("Removed image '{image}'");
        Ok(())
    }

    fn resize_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        new_size: GiB,
    ) -> Result<()> {
        let inst = stopped.instance();
        let (rt, _) = self.qualified_runtime()?;
        let _lock = state::lock_instance(inst)?;
        let sidecar = Self::owned_sidecar(cfg, inst)?;
        let machine = &sidecar.machine_id;
        let before = Self::require_stopped(rt, inst, machine)?;
        let wanted = u64::from(new_size.as_u32()) * GIB;
        if wanted <= before.record.disk_bytes {
            if wanted == before.record.disk_bytes {
                return Ok(());
            }
            bail!(
                "Instance '{}' has a {} GiB disk; shrinking is not supported",
                inst.name,
                before.record.disk_bytes / GIB
            );
        }
        // The runtime grows a scratch clone offline, then publishes it and
        // its new size as one recoverable update: a failure before the swap
        // leaves the disk as it was, and a crash after it is settled by the
        // runtime's next operation on the sandbox. Nothing coop records
        // depends on the disk size, so coop keeps no journal of its own.
        let gib = new_size.to_string();
        let operation = OperationId::generate()?;
        let req = Request::new(
            rt.args(
                &["grow"],
                &[
                    machine.as_str(),
                    "--disk-gib",
                    &gib,
                    "--operation",
                    operation.as_str(),
                ],
            ),
            rt.settings.create,
            MAX_JSON_OUTPUT,
        );
        if let Err(e) = rt.checked(&req) {
            // A grow that reports failure after publishing its update has
            // still changed the disk; only the runtime's record can say, and
            // a record that cannot be read leaves the outcome unknown.
            match rt.inspect(machine) {
                Ok(after) if !after.record.committed(&operation) => return Err(e),
                Ok(_) => bail!(AppleError::OperationUncertain(format!(
                    "growing sandbox {machine} reported failure ({e:#}), but the runtime \
                     committed it; check `coop status {}`",
                    inst.name
                ))),
                Err(inspect_err) => bail!(AppleError::OperationUncertain(format!(
                    "growing sandbox {machine} reported failure ({e:#}) and its record could \
                     not be read ({inspect_err:#}); check `coop status {}`",
                    inst.name
                ))),
            }
        }
        let after = rt.inspect(machine)?;
        if !after.record.committed(&operation) || after.record.disk_bytes != wanted {
            bail!(AppleError::OperationUncertain(format!(
                "sandbox {machine} reports a {} byte disk (last operation {}) after growing to {wanted}",
                after.record.disk_bytes,
                after.record.last_operation_label()
            )));
        }
        tracing::info!("Grew instance '{}' disk to {new_size} GiB", inst.name);
        Ok(())
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
        let (rt, _) = self.qualified_runtime()?;
        let owner = Owner::load(cfg)?;
        let forward = Self::update_resources(
            rt,
            cfg,
            inst,
            &owner,
            |prior| Resources {
                cpus: vcpus.map_or(prior.cpus, |v| u32::from(v.get())),
                memory_bytes: mem.map_or(prior.memory_bytes, |m| mib_to_bytes(m.get().as_u32())),
            },
            None,
        )?;
        if !start_after {
            return Ok(());
        }
        let Err(start_err) = self.start_existing(cfg, inst) else {
            return Ok(());
        };
        // A failed start stops the sandbox it booted; the rollback's own
        // stopped check refuses if that could not be confirmed.
        match Self::update_resources(rt, cfg, inst, &owner, |_| forward.prior, Some(&forward)) {
            Ok(_) => Err(start_err.context(format!(
                "Instance failed to start with the new resources; previous {} restored",
                forward.prior
            ))),
            Err(rb) => bail!(AppleError::OperationUncertain(format!(
                "Instance '{}' failed to start with {} ({start_err:#}), and restoring the previous \
                 {} did not complete ({rb:#}); check `coop status {}` before retrying",
                inst.name, forward.applied, forward.prior, inst.name
            ))),
        }
    }

    /// Save the stopped instance's disk (identity removed) as image `image`.
    fn commit_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        image: &ImageName,
    ) -> Result<()> {
        let inst = stopped.instance();
        let (rt, _) = self.qualified_runtime()?;
        let owner = Owner::load(cfg)?;
        let _lock = state::lock_instance(inst)?;
        let sidecar = Self::owned_sidecar(cfg, inst)?;
        let rec = Self::require_stopped(rt, inst, &sidecar.machine_id)?;
        let disk = MachineName::generate(&owner.id)?;
        let req = Request::new(
            rt.args(&["commit"], &[sidecar.machine_id.as_str(), disk.as_str()]),
            rt.settings.create,
            MAX_JSON_OUTPUT,
        );
        let out = match rt.checked(&req) {
            Ok(out) => out,
            Err(e) => {
                // The runtime may have published the disk before failing;
                // nothing refers to it, so do not leave it behind.
                rt.delete_disk_best_effort(&disk);
                return Err(e.context(format!(
                    "committing '{}' failed; its disk was discarded and no image was saved",
                    inst.name
                )));
            }
        };
        let previous = ImageManifest::load_lenient(cfg, image);
        let saved = protocol::parse_disk(&out).and_then(|committed| {
            ImageManifest {
                schema_version: state::SCHEMA_VERSION,
                backend: state::BACKEND_TAG.into(),
                image_ref: rec.record.image_reference.clone(),
                digest: rec.record.image_digest.clone(),
                disk: Some(CommittedDisk {
                    name: disk.clone(),
                    bytes: committed.logical_bytes,
                }),
                manifest_id: format!("commit-{disk}"),
                base_image: image::BASE_IMAGE.into(),
                platform: image::PLATFORM.into(),
                guest_user: sidecar.guest_user.clone(),
                created: crate::setup::utc_timestamp(),
            }
            .save(cfg, image)
        });
        if let Err(e) = saved {
            // Nothing refers to the new disk yet; do not leave it behind.
            rt.delete_disk_best_effort(&disk);
            return Err(e);
        }
        if let Some(old) = previous {
            release_manifest(rt, cfg, &owner, &old, None);
        }
        Ok(())
    }

    /// Replace the stopped instance's disk with image `image`'s. The new disk
    /// has no host keys; the next start pins the one it generates.
    fn restore_disk(
        &self,
        cfg: &CoopConfig,
        stopped: &StoppedInstance,
        image: &ImageName,
    ) -> Result<()> {
        let inst = stopped.instance();
        let (rt, _) = self.qualified_runtime()?;
        let owner = Owner::load(cfg)?;
        let manifest = ImageManifest::load(cfg, image)?;
        rt.verify_image(&manifest)?;
        let _lock = state::lock_instance(inst)?;
        Self::recover_journal(rt, cfg, inst)?;
        let mut sidecar = Self::owned_sidecar(cfg, inst)?;
        let machine = sidecar.machine_id.clone();
        let rec = Self::require_stopped(rt, inst, &machine)?;
        // SSH logs in as the instance's recorded user; a disk built for
        // another user would leave the instance unable to start.
        if manifest.guest_user != sidecar.guest_user {
            bail!(
                "Image '{image}' was built for guest user '{}', but instance '{}' uses '{}'; \
                 create a new instance from it instead",
                manifest.guest_user,
                inst.name,
                sidecar.guest_user
            );
        }
        let operation = OperationId::generate()?;
        let journaled = JournalOp::RestoreDisk {
            operation: operation.clone(),
            prior_generation: rec.record.disk_generation,
        };
        Journal::begin(inst, &owner, journaled.clone(), machine.clone())?;
        let mut rest: Vec<&str> = match &manifest.disk {
            Some(d) => vec![machine.as_str(), d.name.as_str()],
            None => vec![machine.as_str(), "--image", manifest.image_ref.as_str()],
        };
        rest.extend(["--operation", operation.as_str()]);
        let req = Request::new(
            rt.args(&["restore"], &rest),
            rt.settings.create,
            MAX_JSON_OUTPUT,
        );
        rt.checked(&req)?;
        let after = rt.inspect(&machine)?;
        if after.record.disk_generation <= rec.record.disk_generation
            || !after.record.committed(&operation)
        {
            bail!(AppleError::OperationUncertain(format!(
                "sandbox {machine} did not report this restore as its disk; {}",
                journaled.recovery_hint(&inst.name)
            )));
        }
        sidecar.image_ref.clone_from(&after.record.image_reference);
        sidecar.image_digest.clone_from(&after.record.image_digest);
        sidecar.image_manifest_id.clone_from(&manifest.manifest_id);
        sidecar.reenroll_host_key = true;
        sidecar.save(inst)?;
        Journal::complete(inst)
    }

    fn is_running(&self, inst: &Instance) -> bool {
        let Ok(Some(sidecar)) = MachineSidecar::try_load(inst) else {
            return false;
        };
        self.runtime()
            .and_then(|rt| rt.inspect(&sidecar.machine_id))
            .is_ok_and(|rec| rec.status == SandboxStatus::Running)
    }

    fn images_in_data_dir(&self) -> bool {
        false
    }

    fn probe_running(&self, inst: &Instance) -> Result<bool> {
        if let Some(journal) = Journal::try_load(inst)? {
            bail!(AppleError::OperationUncertain(format!(
                "instance '{}' has an unfinished {}",
                inst.name,
                journal.op.describe()
            )));
        }
        let Some(sidecar) = MachineSidecar::try_load(inst)? else {
            return Ok(false);
        };
        let rec = self.runtime()?.inspect(&sidecar.machine_id)?;
        match rec.status {
            SandboxStatus::Running => Ok(true),
            // A crashed owner left no VM, and `start` accepts it.
            SandboxStatus::Stopped | SandboxStatus::Crashed => Ok(false),
            SandboxStatus::Booting => bail!(AppleError::OperationUncertain(format!(
                "sandbox {} is {}",
                sidecar.machine_id,
                rec.status.label()
            ))),
        }
    }

    fn as_running(&self, cfg: &CoopConfig, inst: Instance) -> Result<Option<RunningInstance>> {
        let sidecar = Self::owned_sidecar(cfg, &inst)?;
        let rt = self.runtime()?;
        let rec = rt.inspect(&sidecar.machine_id)?;
        match rec.status {
            SandboxStatus::Stopped => Ok(None),
            SandboxStatus::Running => {
                // A running sandbox that cannot be handed out (unqualified
                // runtime, failed gate) is an error, never "not running":
                // callers must not report it stopped.
                let target = self
                    .target_for(cfg, &inst, &sidecar, &rec)
                    .with_context(|| {
                        format!(
                            "Instance '{}' is running but cannot be reached safely; \
                             `coop stop {}` stops it without connecting to the guest",
                            inst.name, inst.name
                        )
                    })?;
                Ok(Some(RunningInstance::new(inst, target)))
            }
            other => bail!(AppleError::OperationUncertain(format!(
                "sandbox {} is {}",
                sidecar.machine_id,
                other.label()
            ))),
        }
    }

    fn as_stopped(&self, inst: Instance) -> Result<StoppedInstance> {
        let sidecar = MachineSidecar::load(&inst)?;
        let rec = self.runtime()?.inspect(&sidecar.machine_id)?;
        match rec.status {
            SandboxStatus::Stopped => Ok(StoppedInstance::new(inst)),
            SandboxStatus::Running => bail!(
                "Instance '{}' is running — stop it first with `coop stop {}`",
                inst.name,
                inst.name,
            ),
            other => bail!(AppleError::OperationUncertain(format!(
                "sandbox {} is {}",
                sidecar.machine_id,
                other.label()
            ))),
        }
    }

    fn status(&self, cfg: &CoopConfig, running: &RunningInstance) -> Result<String> {
        use std::fmt::Write as _;
        let inst = running.instance();
        let sidecar = Self::owned_sidecar(cfg, inst)?;
        let rt = self.runtime()?;
        let rec = rt.inspect(&sidecar.machine_id)?;
        let runtime = rt
            .qualification
            .as_ref()
            .map_or_else(|e| format!("unqualified ({e})"), |q| q.identity.clone());
        let mut out = describe_sandbox(inst, &sidecar, &rec, &runtime);
        match crate::backend::query_resource_usage(running.target()) {
            Some(usage) => {
                let _ = write!(out, "\n  {usage}");
            }
            None => out.push_str("\n  Guest usage: unavailable"),
        }
        Ok(out)
    }

    fn stream_logs(
        &self,
        cfg: &CoopConfig,
        running: &RunningInstance,
        mode: LogMode,
    ) -> Result<()> {
        use std::io::Write as _;
        let sidecar = Self::owned_sidecar(cfg, running.instance())?;
        let rt = self.runtime()?;
        let name = sidecar.machine_id.as_str();
        match mode {
            LogMode::Follow => {
                // Guest-controlled console output: replace control bytes on
                // every line before it reaches the operator's terminal.
                let args = rt.args(&["logs"], &[name, "--follow"]);
                let mut stdout = std::io::stdout().lock();
                let out = rt.exec.run_streaming(&args, &mut |line| {
                    writeln!(
                        stdout,
                        "{}",
                        cli::sanitize_for_display(&String::from_utf8_lossy(line))
                    )
                    .context("Failed to write logs")
                })?;
                if !out.success() {
                    bail!("`coop-sandbox logs` exited with {:?}", out.code);
                }
            }
            LogMode::Snapshot => {
                // Spool to a private file rather than memory, then print it
                // line by line with guest-controlled control bytes replaced.
                let spool = tempfile::NamedTempFile::new().context("Failed to create log spool")?;
                let req = Request::new(rt.args(&["logs"], &[name]), rt.settings.boot, 0);
                let out = rt.exec.run_logged(&req, spool.path())?;
                if !out.success() {
                    bail!(
                        "`coop-sandbox logs` failed: {}",
                        cli::log_tail(spool.path(), 2048)
                    );
                }
                let reader = std::io::BufReader::new(
                    std::fs::File::open(spool.path()).context("Failed to read log spool")?,
                );
                let mut stdout = std::io::stdout().lock();
                cli::for_each_bounded_line(reader, cli::MAX_LOG_LINE, &mut |line| {
                    writeln!(
                        stdout,
                        "{}",
                        cli::sanitize_for_display(&String::from_utf8_lossy(line))
                    )
                    .context("Failed to write logs")
                })?;
            }
        }
        Ok(())
    }

    fn ssh_target(&self, cfg: &CoopConfig, inst: &Instance) -> Result<SshTarget> {
        let sidecar = Self::owned_sidecar(cfg, inst)?;
        let rec = self.runtime()?.inspect(&sidecar.machine_id)?;
        self.target_for(cfg, inst, &sidecar, &rec)
    }

    /// The sandbox's disk file; its size is the instance's disk size.
    fn disk_path(&self, inst: &Instance) -> Result<PathBuf> {
        let sidecar = MachineSidecar::load(inst)?;
        Ok(security::rootfs_path(
            &canonical_path(&self.settings.runtime_root),
            &sidecar.machine_id,
        ))
    }

    fn local_endpoint_route(&self, _network: &NetworkConfig) -> LocalEndpointRoute {
        LocalEndpointRoute::ReverseTunnel
    }

    fn image_is_built(&self, cfg: &CoopConfig, image: &ImageName) -> bool {
        let Ok(Some(manifest)) = ImageManifest::try_load(cfg, image) else {
            return false;
        };
        self.runtime()
            .and_then(|rt| rt.verify_image(&manifest))
            .is_ok()
    }
}

/// `coop status` text for a sandbox, from the runtime's record.
fn describe_sandbox(
    inst: &Instance,
    sidecar: &MachineSidecar,
    rec: &Inspect,
    runtime: &str,
) -> String {
    let ip = rec
        .ip()
        .map_or_else(|| "unavailable".to_string(), |ip| ip.to_string());
    let network = rec
        .effective
        .as_ref()
        .and_then(|e| e.interfaces.first())
        .map_or_else(
            || "unavailable".to_string(),
            |i| cli::sanitize_for_display(&i.network),
        );
    let gib = |b: u64| {
        let tenths = b * 10 / GIB;
        format!("{}.{}", tenths / 10, tenths % 10)
    };
    format!(
        "Instance '{}' ({})\n\
         \x20 Backend: apple-container (coop-sandbox)\n\
         \x20 Runtime: {runtime}\n\
         \x20 Sandbox: {}\n\
         \x20 Network: {network} (dedicated)\n\
         \x20 vCPUs: {}\n\
         \x20 Memory: {} MiB\n\
         \x20 Disk: {} GiB ({} GiB allocated)\n\
         \x20 Address: {ip} (host key {})\n\
         \x20 Workspace: copied (no live mounts)",
        inst.name,
        rec.status.label(),
        sidecar.machine_id,
        rec.record.cpus,
        rec.record.memory_bytes / MIB,
        gib(rec.disk.logical_bytes),
        gib(rec.disk.allocated_bytes),
        sidecar.host_key_fingerprint,
    )
}

/// The backend-agnostic half of a freshly built image.
fn save_template_config(cfg: &CoopConfig, opts: &SetupOptions, manifest_id: &str) -> Result<()> {
    crate::setup::TemplateConfig {
        version: crate::setup::TEMPLATE_VERSION,
        created: crate::setup::utc_timestamp(),
        install_script_hash: crate::sha256_hash::Sha256Hash::of(manifest_id),
        profiles: opts.profiles.iter().map(|p| p.name.clone()).collect(),
        extra_packages: Vec::new(),
        post_install_hash: None,
        // Nothing is baked: the first boot installs every marketplace,
        // plugin, and MCP server through the shared bootstrap.
        marketplaces: Vec::new(),
        plugins: Vec::new(),
        codex_marketplaces: Vec::new(),
        codex_plugins: Vec::new(),
        guest_user: opts.guest_user.clone(),
        oci_features: crate::devcontainer_oci::installed_features(&opts.oci_features),
    }
    .save_for(cfg, &opts.image)
}

fn remove_instance_dir(inst: &Instance) -> Result<()> {
    if inst.dir.exists() {
        std::fs::remove_dir_all(&inst.dir)
            .with_context(|| format!("Failed to remove {}", inst.dir.display()))?;
    }
    Ok(())
}

/// Generate the VM-access keypair under the backend root if missing.
fn ensure_ssh_key(cfg: &CoopConfig) -> Result<()> {
    let key_path = cfg.ssh_key_path();
    if key_path.exists() {
        return Ok(());
    }
    crate::cmd::Cmd::new("/usr/bin/ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-C", "coop-apple", "-f"])
        .arg(&key_path)
        .run()
        .context("Failed to generate the VM access key")
}

/// Build `image_ref` from `context` with the stock builder (output to `log`),
/// move it into the runtime's store, then verify it in a disposable sandbox.
/// Returns the digest.
#[expect(clippy::too_many_arguments, reason = "one build, all inputs explicit")]
fn build_import_and_verify(
    rt: &Runtime,
    builder: &Builder,
    cfg: &CoopConfig,
    owner: &Owner,
    image_ref: &str,
    context: &tempfile::TempDir,
    log: &Path,
    timeout: Duration,
    guest_user: &crate::guest::GuestUser,
) -> Result<String> {
    let digest = build_into_runtime(rt, builder, image_ref, context.path(), log, timeout)?;
    verify_image_in_sandbox(rt, cfg, owner, image_ref, guest_user)?;
    Ok(digest)
}

/// Build `image_ref` from `context` with the stock builder (output to
/// `log`) and move it into the runtime's store, deleting the builder's copy.
/// Returns the digest.
fn build_into_runtime(
    rt: &Runtime,
    builder: &Builder,
    image_ref: &str,
    context: &Path,
    log: &Path,
    timeout: Duration,
) -> Result<String> {
    let build = Request::new(
        [
            "build".to_string(),
            "--platform".into(),
            image::PLATFORM.into(),
            "--progress".into(),
            "plain".into(),
            "-t".into(),
            image_ref.to_string(),
            context.display().to_string(),
        ],
        timeout,
        0,
    )
    .cancellable();
    let out = builder.exec.run_logged(&build, log)?;
    if !out.success() {
        bail!(
            "Image build failed. Last lines of {}:\n{}",
            log.display(),
            cli::log_tail(log, 4096)
        );
    }
    // The OCI archive holds only the image just built from the private context.
    let archive = tempfile::Builder::new()
        .prefix("coop-apple-image-")
        .tempdir()
        .context("Failed to create image export directory")?;
    let tar = archive.path().join("image.tar");
    let tar_arg = tar.display().to_string();
    let saved = builder.text(
        &[
            "image",
            "save",
            "--platform",
            image::PLATFORM,
            "-o",
            &tar_arg,
            image_ref,
        ],
        rt.settings.create,
    );
    // The builder's copy is not used again, whatever happens next.
    let _ = builder.text(&["image", "delete", image_ref], rt.settings.operation);
    saved?;
    let imported = protocol::parse_images(&rt.text(
        rt.args(&["image", "import"], &["--oci-tar", &tar_arg]),
        rt.settings.create,
        MAX_JSON_OUTPUT,
    )?)?;
    let Some(entry) = imported.iter().find(|i| i.reference == image_ref) else {
        bail!(AppleError::IdentityConflict(format!(
            "importing {image_ref} into the runtime produced {:?}",
            imported.iter().map(|i| &i.reference).collect::<Vec<_>>()
        )));
    };
    Ok(entry.digest.clone())
}

/// The image tags and committed disks the saved manifests (other than
/// `except`) still use, or `None` when one cannot be read: nothing is
/// released then, since it might be in use.
fn manifest_references(
    cfg: &CoopConfig,
    except: Option<&ImageName>,
) -> Option<(HashSet<String>, HashSet<String>)> {
    let images = cfg
        .list_images()
        .map_err(|e| tracing::warn!("Keeping image content: cannot list images: {e:#}"))
        .ok()?;
    let (mut refs, mut disks) = (HashSet::new(), HashSet::new());
    for info in images.iter().filter(|i| Some(&i.name) != except) {
        match ImageManifest::try_load(cfg, &info.name) {
            Ok(Some(m)) => {
                if let Some(d) = &m.disk {
                    disks.insert(d.name.to_string());
                }
                refs.insert(m.image_ref);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    "Keeping image content: manifest for '{}' is unreadable: {e:#}",
                    info.name
                );
                return None;
            }
        }
    }
    Some((refs, disks))
}

/// Delete an owned manifest's runtime content (its committed disk and its
/// image tag) unless a saved manifest other than `except` still uses it.
/// Instances never depend on either after creation.
fn release_manifest(
    rt: &Runtime,
    cfg: &CoopConfig,
    owner: &Owner,
    manifest: &ImageManifest,
    except: Option<&ImageName>,
) {
    let Some((refs, disks)) = manifest_references(cfg, except) else {
        return;
    };
    if let Some(disk) = &manifest.disk
        && disk.name.belongs_to(&owner.id)
        && !disks.contains(disk.name.as_str())
    {
        rt.delete_disk_best_effort(&disk.name);
    }
    if image::is_owned_ref(owner, &manifest.image_ref) && !refs.contains(&manifest.image_ref) {
        rt.delete_image_best_effort(&manifest.image_ref);
    }
}

/// Boot the candidate image in a disposable, owned sandbox with no
/// credentials, check the guest contract, and always clean up.
fn verify_image_in_sandbox(
    rt: &Runtime,
    cfg: &CoopConfig,
    owner: &Owner,
    image_ref: &str,
    guest_user: &crate::guest::GuestUser,
) -> Result<()> {
    let machine = MachineName::generate(&owner.id)?;
    tracing::info!("Verifying image in disposable sandbox {machine}");
    // Small and fixed: the check needs a boot, not the instance's resources.
    let (cpus, memory_mib) = (2u8, 2048u32);
    let expected = security::Expected {
        sandbox: &machine,
        owner: &owner.id,
        runtime_root: &rt.root,
        resources: Resources {
            cpus: u32::from(cpus),
            memory_bytes: mib_to_bytes(memory_mib),
        },
    };
    let result = (|| -> Result<()> {
        // The instance default disk size, so the unpacked base is cached for
        // the first `coop up`.
        rt.create(
            &machine,
            &Source::Image(image_ref),
            NonZeroU8::new(cpus).context("verification vCPUs")?,
            memory_mib,
            cfg.vm.template_size_gib,
            owner,
        )?;
        security::verify_record(&rt.inspect(&machine)?, &expected)?;
        // A disposable sandbox: its key is read to prove the guest booted
        // fully, and never pinned.
        rt.boot_validated(&expected, Instant::now() + rt.settings.boot)?;
        let wanted: Vec<String> = crate::guest::required_guest_binaries(guest_user)
            .iter()
            .map(ToString::to_string)
            .collect();
        let missing = rt.missing_executables(&machine, &wanted)?;
        crate::guest::verify_required_binaries(
            guest_user,
            "image build",
            |path| !missing.iter().any(|m| m == path.as_ref()),
            String::new,
        )?;
        // The guest user is baked into the image; coop's forwards assume uid 1000.
        let uid = rt.guest(
            &machine,
            &["/usr/bin/id", "-u", guest_user.as_str()],
            rt.settings.probe,
            MAX_TEXT_OUTPUT,
        )?;
        if uid.stdout_str().map(str::trim).ok() != Some("1000") {
            bail!("Image verification failed: guest user {guest_user} is not uid 1000");
        }
        // Services get their own window: docker is often still starting
        // when the boot deadline's readiness checks have finished.
        rt.wait_for_services(
            &machine,
            &["ssh", "docker"],
            Instant::now() + rt.settings.boot,
        )?;
        Ok(())
    })();
    let cleanup = (|| -> Result<()> {
        if rt.exists(&machine)? {
            let rec = rt.inspect(&machine)?;
            if rec.status != SandboxStatus::Stopped {
                rt.stop_and_confirm(&machine)?;
            }
            rt.delete(&machine, owner)?;
        }
        Ok(())
    })();
    if let Err(e) = &cleanup {
        tracing::warn!("Failed to clean up verification sandbox {machine}: {e:#}");
    }
    result
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests;
