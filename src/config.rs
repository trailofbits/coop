use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::marker::PhantomData;
use std::net::Ipv4Addr;
use std::num::{NonZeroU8, NonZeroU16, NonZeroU32};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cmd::Cmd;
use crate::guest_env_state::EnvVarName;
use crate::naming::validate_safe_chars;
use crate::paths::GuestPath;

pub const DEFAULT_IMAGE: &str = "default";

// ── Command substitution for secrets ─────────────────────────

const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve a config value that may use `cmd:` prefix for secret
/// manager integration.
///
/// If the value starts with `cmd:`, the remainder is executed as
/// a shell command and its trimmed stdout is returned. Plain values
/// pass through unchanged. Commands that fail, timeout, or produce
/// empty output return an error.
pub(crate) fn resolve_cmd_value(value: &str) -> Result<String> {
    let cmd_str = match value.strip_prefix("cmd:") {
        Some(cmd) => cmd.trim(),
        None => return Ok(value.to_string()),
    };

    if cmd_str.is_empty() {
        bail!("Empty command after 'cmd:' prefix");
    }

    tracing::debug!("Resolving secret via command");

    let mut child = Command::new("sh")
        .args(["-c", cmd_str])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to spawn secret command: {cmd_str}"))?;

    let start = Instant::now();

    loop {
        if child
            .try_wait()
            .context("Failed to check command status")?
            .is_some()
        {
            let output = child
                .wait_with_output()
                .with_context(|| format!("Failed to read output: {cmd_str}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let code = output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_string(), |c| c.to_string());
                bail!(
                    "Secret command failed (exit {code}): {cmd_str}\n\
                     stderr: {stderr}"
                );
            }

            let stdout = String::from_utf8(output.stdout)
                .context("Secret command output is not valid UTF-8")?;
            let resolved = stdout.trim().to_string();

            if resolved.is_empty() {
                bail!(
                    "Secret command produced empty output: {cmd_str}\n\
                     The command succeeded but stdout was empty after trimming."
                );
            }

            return Ok(resolved);
        }

        if start.elapsed() >= CMD_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "Secret command timed out after {}s: {cmd_str}\n\
                 Ensure the command runs non-interactively.",
                CMD_TIMEOUT.as_secs(),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ── Secret wrapper ───────────────────────────────────────────

/// Generic wrapper for values that must not appear in `Debug` output.
///
/// `Debug` always prints `<redacted>`, so types embedded in error chains,
/// `tracing` events, panic messages, or `dbg!` never leak the value.
/// `Display` is intentionally **not** implemented — printing the secret
/// must be an explicit `.expose()` call, which greps cleanly during review.
///
/// Round-trips through serde transparently: a config file with
/// `api_key = "sk-…"` deserializes to `Secret(String::from("sk-…"))`,
/// and re-serializing produces the same value. If a config-dump command
/// is added later, redaction belongs at the formatter for that command,
/// not on this type.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Borrow the underlying value. Named to flag every read at review time.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

// ── Newtypes ─────────────────────────────────────────────────

/// Marker types and trait for [`Quantity`] — the only place a new
/// unit is introduced. Adding a unit means: a marker struct, an
/// `impl Unit for ...` with the human-facing suffix, and a `type` alias.
pub trait Unit: Copy + fmt::Debug + 'static {
    /// Human-readable suffix used in CLI/parser error messages.
    const SUFFIX: &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MibUnit;
impl Unit for MibUnit {
    const SUFFIX: &'static str = "MiB";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GibUnit;
impl Unit for GibUnit {
    const SUFFIX: &'static str = "GiB";
}

/// Non-zero byte-scaled quantity. The phantom unit parameter prevents
/// silently mixing `MiB` and `GiB`; the inner `NonZeroU32` rejects
/// zero at deserialization time so no runtime check is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Quantity<U: Unit>(NonZeroU32, PhantomData<U>);

/// Memory size in mebibytes.
pub type MiB = Quantity<MibUnit>;
/// Disk/storage size in gibibytes.
pub type GiB = Quantity<GibUnit>;

impl<U: Unit> Quantity<U> {
    /// Create from a runtime value. Returns `None` if zero.
    pub fn new(value: u32) -> Option<Self> {
        NonZeroU32::new(value).map(|n| Self(n, PhantomData))
    }

    /// Wrap an existing `NonZeroU32` — infallible because the inner
    /// invariant already holds. Use this to bridge from other
    /// non-zero-backed types without round-tripping through `Option`,
    /// and to build `const` quantity values from non-zero literals.
    pub const fn from_nonzero(value: NonZeroU32) -> Self {
        Self(value, PhantomData)
    }

    /// Clap value parser: accept a positive integer string, reject zero.
    pub fn parse_cli(s: &str) -> Result<Self> {
        let n: u32 = s
            .parse()
            .with_context(|| format!("expected positive integer {}, got '{s}'", U::SUFFIX))?;
        Self::new(n).with_context(|| format!("{} must be > 0, got '{s}'", U::SUFFIX))
    }

    pub fn as_u32(self) -> u32 {
        self.0.get()
    }

    pub fn as_nonzero(self) -> NonZeroU32 {
        self.0
    }
}

impl<U: Unit> fmt::Display for Quantity<U> {
    #[mutants::skip] // equivalent: callers don't assert the formatted output, only round-trip via parse_cli
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<U: Unit> Serialize for Quantity<U> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, U: Unit> Deserialize<'de> for Quantity<U> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        NonZeroU32::deserialize(d).map(Self::from_nonzero)
    }
}

impl Quantity<MibUnit> {
    /// MiB → GiB as floating-point (1024 MiB = 1 GiB).
    pub fn as_gib_f64(self) -> f64 {
        f64::from(self.0.get()) / 1024.0
    }
}

/// Smallest guest memory that boots reliably. Below this the kernel
/// fails to come up, so it is the floor enforced by [`VmMemory::new`].
pub const MIN_MEM_MIB: MiB = MiB::from_nonzero(NonZeroU32::new(128).unwrap());

/// Guest RAM that is provably bootable: at least [`MIN_MEM_MIB`].
///
/// `MiB` is a generic byte quantity whose only invariant is non-zero;
/// the 128 MiB floor is domain-specific to *VM memory*, so it lives here
/// rather than on `MiB`. Every entry point that sets guest memory — the
/// `--mem` CLI flag, `config.toml`, `coop resize`, and the devcontainer
/// translator — constructs through [`Self::new`], so no path can hold an
/// unbootable value. This is parse-don't-validate: the floor is a
/// property of the type, not a check a caller must remember to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VmMemory(MiB);

impl VmMemory {
    /// Wrap a `MiB`, rejecting anything below [`MIN_MEM_MIB`].
    pub fn new(mib: MiB) -> Result<Self> {
        if mib < MIN_MEM_MIB {
            bail!("mem_size_mib={mib} is too low (minimum {MIN_MEM_MIB})");
        }
        Ok(Self(mib))
    }

    /// Clap value parser for `--mem`: a positive integer at or above the floor.
    pub fn parse_cli(s: &str) -> Result<Self> {
        Self::new(MiB::parse_cli(s)?)
    }

    /// The underlying mebibyte quantity.
    pub fn get(self) -> MiB {
        self.0
    }
}

impl fmt::Display for VmMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for VmMemory {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for VmMemory {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mib = MiB::deserialize(d)?;
        Self::new(mib).map_err(serde::de::Error::custom)
    }
}

/// Disk size specification: absolute (`150`) or relative (`+20`), in GiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskSize {
    Absolute(GiB),
    Relative(GiB),
}

impl DiskSize {
    /// Parse from a CLI string like `"150"`, `"150G"`, `"+20"`, or `"+20G"`.
    pub fn parse(s: &str) -> Result<Self> {
        let (relative, rest) = if let Some(r) = s.strip_prefix('+') {
            (true, r)
        } else {
            (false, s)
        };
        let numeric = rest
            .strip_suffix('G')
            .or_else(|| rest.strip_suffix('g'))
            .unwrap_or(rest);
        let value: u32 = numeric
            .parse()
            .with_context(|| format!("Invalid disk size: {s}"))?;
        let gib = GiB::new(value).with_context(|| format!("Disk size must be > 0: {s}"))?;
        if relative {
            Ok(Self::Relative(gib))
        } else {
            Ok(Self::Absolute(gib))
        }
    }

    /// Resolve to an absolute GiB value given the current disk size.
    ///
    /// Both inputs are non-zero, so the result is too; the only
    /// remaining failure mode is `u32` overflow on a relative add.
    pub fn resolve(self, current: GiB) -> Result<GiB> {
        match self {
            Self::Absolute(gib) => Ok(gib),
            Self::Relative(gib) => current
                .as_nonzero()
                .checked_add(gib.as_u32())
                .map(GiB::from_nonzero)
                .context("Disk size overflow"),
        }
    }
}

/// Instance index (0..=252), used to derive guest IP, TAP name, MAC, and vsock CID.
///
/// The bound exists because the guest IP is `172.16.0.<idx + 2>`: index 252
/// maps to `172.16.0.254`, leaving `.255` as the broadcast address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "u16", try_from = "u16")]
pub struct InstanceIndex(u16);

impl InstanceIndex {
    /// Largest valid index. See struct docs for the rationale.
    pub const MAX: u16 = 252;

    /// Create from a runtime value. Returns `None` if greater than [`Self::MAX`].
    pub fn new(value: u16) -> Option<Self> {
        (value <= Self::MAX).then_some(Self(value))
    }

    pub fn as_u16(self) -> u16 {
        self.0
    }

    pub fn as_u32(self) -> u32 {
        u32::from(self.0)
    }
}

impl fmt::Display for InstanceIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<InstanceIndex> for u16 {
    fn from(idx: InstanceIndex) -> Self {
        idx.0
    }
}

/// Error returned when a raw `u16` exceeds [`InstanceIndex::MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceIndexOutOfRange(pub u16);

impl fmt::Display for InstanceIndexOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "instance index {} out of range 0..={}",
            self.0,
            InstanceIndex::MAX
        )
    }
}

impl std::error::Error for InstanceIndexOutOfRange {}

impl TryFrom<u16> for InstanceIndex {
    type Error = InstanceIndexOutOfRange;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(InstanceIndexOutOfRange(value))
    }
}

/// A host directory to mount into the guest VM.
#[derive(Debug, Clone)]
pub struct Mount {
    pub host_path: PathBuf,
    pub guest_path: GuestPath,
}

impl Mount {
    /// Parse a mount spec in the form `HOST_PATH[:GUEST_PATH]`.
    ///
    /// If `GUEST_PATH` is omitted, defaults to `/workspace`.
    pub fn parse(spec: &str) -> Result<Self> {
        let (host, guest_path) = if let Some((h, g)) = spec.split_once(':') {
            (h, GuestPath::absolute(g)?)
        } else {
            (spec, GuestPath::absolute("/workspace")?)
        };
        Self::from_parts(host, guest_path)
    }

    /// Build a `Mount` from already-split host and guest components.
    ///
    /// Single source of truth for the canonicalize / is-dir
    /// invariants; the absolute-guest invariant is carried by
    /// `GuestPath::absolute` at the type boundary. Callers that build
    /// the spec from typed fields (devcontainer JSON, Docker
    /// `type=bind` form) skip the string round-trip by calling this
    /// directly.
    pub fn from_parts(host: &str, guest_path: GuestPath) -> Result<Self> {
        let host_path = Path::new(host)
            .canonicalize()
            .with_context(|| format!("Mount host path does not exist: {host}"))?;

        anyhow::ensure!(
            host_path.is_dir(),
            "Mount host path is not a directory: {}",
            host_path.display()
        );

        Ok(Self {
            host_path,
            guest_path,
        })
    }

    /// True if the mount source contains a `.git` entry (regular repo or
    /// linked worktree). Used to warn users that live-mounting a repo
    /// risks the guest writing absolute `/workspace` paths into the
    /// shared `.git/config`.
    pub fn host_is_git_repo(&self) -> bool {
        self.host_path.join(".git").exists()
    }
}

/// Reject a mount set in which two mounts target the same guest path.
pub(crate) fn validate_unique_guest_paths(mounts: &[Mount]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for mount in mounts {
        let guest_path = mount.guest_path.to_string();
        if !seen.insert(guest_path.clone()) {
            bail!("Duplicate mount guest path: {guest_path}");
        }
    }
    Ok(())
}

/// A guest port to forward to the host for the lifetime of the VM.
///
/// Construction normalizes the spec so downstream code (SSH `-L` flags)
/// sees a canonical `(guest, host)` pair where `host` defaults to
/// `guest` when omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForward {
    pub guest: NonZeroU16,
    pub host: NonZeroU16,
    pub label: Option<String>,
}

impl PortForward {
    /// Parse a CLI spec in the form `GUEST[:HOST]`.
    ///
    /// `--forward-port 3000` ⇒ guest=3000, host=3000.
    /// `--forward-port 3000:3001` ⇒ guest=3000, host=3001.
    pub fn parse(spec: &str) -> Result<Self> {
        let (guest_str, host_str) = match spec.split_once(':') {
            Some((g, h)) => (g.trim(), Some(h.trim())),
            None => (spec.trim(), None),
        };
        let guest: u16 = guest_str.parse().with_context(|| {
            format!("Invalid forward-port spec '{spec}': guest port must be a number 1..=65535")
        })?;
        let guest = NonZeroU16::new(guest).with_context(|| {
            format!("Invalid forward-port spec '{spec}': guest port must be > 0")
        })?;
        let host = match host_str {
            Some(s) => {
                let h: u16 = s.parse().with_context(|| {
                    format!(
                        "Invalid forward-port spec '{spec}': host port must be a number 1..=65535"
                    )
                })?;
                NonZeroU16::new(h).with_context(|| {
                    format!("Invalid forward-port spec '{spec}': host port must be > 0")
                })?
            }
            None => guest,
        };
        Ok(Self {
            guest,
            host,
            label: None,
        })
    }
}

/// TOML deserializer for `PortForward`. Accepts:
///
/// - an integer: `3000` ⇒ guest=3000, host=3000
/// - a string: `"3000"` or `"3000:3001"` (CLI form)
/// - a table: `{ guest = 3000, host = 3001, label = "dev" }`
impl<'de> Deserialize<'de> for PortForward {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, Visitor};

        struct PortForwardVisitor;

        impl<'de> Visitor<'de> for PortForwardVisitor {
            type Value = PortForward;

            #[mutants::skip] // equivalent: serde Visitor::expecting is only used in error messages, not asserted
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a port number, a 'GUEST[:HOST]' string, or a { guest, host, label } table",
                )
            }

            fn visit_u64<E: Error>(self, v: u64) -> Result<Self::Value, E> {
                let port: u16 = v
                    .try_into()
                    .map_err(|_| E::custom(format!("port {v} out of range 1..=65535")))?;
                let port = NonZeroU16::new(port).ok_or_else(|| E::custom("port must be > 0"))?;
                Ok(PortForward {
                    guest: port,
                    host: port,
                    label: None,
                })
            }

            fn visit_i64<E: Error>(self, v: i64) -> Result<Self::Value, E> {
                let v: u64 = v
                    .try_into()
                    .map_err(|_| E::custom(format!("port {v} must be > 0")))?;
                self.visit_u64(v)
            }

            fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
                PortForward::parse(v).map_err(E::custom)
            }

            fn visit_string<E: Error>(self, v: String) -> Result<Self::Value, E> {
                self.visit_str(&v)
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                #[derive(Deserialize)]
                #[serde(field_identifier, rename_all = "snake_case")]
                enum Field {
                    Guest,
                    Host,
                    Label,
                    #[serde(other)]
                    Unknown,
                }

                let mut guest: Option<NonZeroU16> = None;
                let mut host: Option<NonZeroU16> = None;
                let mut label: Option<String> = None;
                while let Some(field) = map.next_key::<Field>()? {
                    match field {
                        Field::Guest => guest = Some(map.next_value()?),
                        Field::Host => host = Some(map.next_value()?),
                        Field::Label => label = Some(map.next_value()?),
                        Field::Unknown => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                let guest =
                    guest.ok_or_else(|| M::Error::custom("forward_ports entry missing 'guest'"))?;
                Ok(PortForward {
                    guest,
                    host: host.unwrap_or(guest),
                    label,
                })
            }
        }

        deserializer.deserialize_any(PortForwardVisitor)
    }
}

impl Serialize for PortForward {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("guest", &self.guest)?;
        if self.host != self.guest {
            map.serialize_entry("host", &self.host)?;
        }
        if let Some(label) = &self.label {
            map.serialize_entry("label", label)?;
        }
        map.end()
    }
}

/// Merge `[forward_ports]` from config with the CLI's `--forward-port` flag.
///
/// Walks both lists in order; on a duplicate guest port, the later entry wins.
/// CLI entries are appended after config entries, so CLI overrides config.
pub fn merge_forward_ports(
    config_forwards: &[PortForward],
    cli_forwards: &[PortForward],
) -> Vec<PortForward> {
    let mut out: Vec<PortForward> = Vec::new();
    for f in config_forwards.iter().chain(cli_forwards.iter()) {
        if let Some(existing) = out.iter_mut().find(|e| e.guest == f.guest) {
            *existing = f.clone();
        } else {
            out.push(f.clone());
        }
    }
    out
}

/// A host filesystem path declared in `config.toml`, with a leading `~`
/// expanded to the home directory at construction time.
///
/// A shell expands `~` before a program sees its command-line arguments,
/// but values read from `config.toml` reach us verbatim — the shell never
/// touches them. Wrapping every scalar path field in this newtype moves
/// the expansion into the type system: it happens once, by construction,
/// on *every* deserialization path (`load`, a bare `toml::from_str`, the
/// fuzz target), and a newly added path field is expanded automatically
/// just by using the type. There is no central expansion function to
/// remember to update — the bug class #349 fixed cannot reappear.
///
/// Distinct from `paths::HostPath`, which is a direction marker
/// (host vs. guest) for scp/rsync call sites and carries no invariant.
/// This type is about the tilde-expansion invariant on config input, so
/// it is a separate concept rather than a reuse of that marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigPath(PathBuf);

impl ConfigPath {
    /// Wrap a path, expanding a leading `~` to the home directory. The
    /// single place the expansion invariant is established; the
    /// [`Deserialize`] impl routes through here too.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self(crate::shell::expand_tilde(path.as_ref()))
    }
}

impl std::ops::Deref for ConfigPath {
    type Target = Path;

    #[mutants::skip] // equivalent: trivial forwarder; the wrapped path is asserted via ConfigPath equality
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for ConfigPath {
    #[mutants::skip] // equivalent: trivial forwarder to the wrapped path
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for ConfigPath {
    #[mutants::skip] // equivalent: trivial forwarder to the wrapped path
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl<'de> Deserialize<'de> for ConfigPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::new(PathBuf::deserialize(deserializer)?))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CoopConfig {
    /// Directory for storing VM artifacts (images, sockets, logs)
    #[serde(default = "default_data_dir")]
    pub data_dir: ConfigPath,

    #[serde(default)]
    pub vm: VmConfig,
    #[serde(default)]
    pub network: NetworkConfig,

    /// SSH port on the guest
    #[serde(default = "default_ssh_port")]
    pub ssh_port: NonZeroU16,

    /// Path to firecracker binary
    #[serde(default = "default_firecracker_bin")]
    pub firecracker_bin: ConfigPath,

    /// GitHub auth strategy for the guest
    #[serde(default)]
    pub github: Option<GitHubAuth>,

    /// Setup-time UX behaviour
    #[serde(default)]
    pub setup: SetupConfig,

    /// Claude Code config forwarding settings
    #[serde(default)]
    pub claude: ClaudeConfig,

    /// Codex config forwarding settings
    #[serde(default)]
    pub codex: CodexConfig,

    /// Literal env vars to set in the guest, independent of the host
    /// process environment. Merged with `env_forward` results during
    /// SSH setup; entries here override forwarded values (with a
    /// `tracing::warn!`).
    ///
    /// `BTreeMap` for deterministic iteration order — useful for
    /// snapshot/diagnostic stability.
    #[serde(default)]
    pub guest_env: BTreeMap<crate::guest_env_state::EnvVarName, String>,

    /// User-defined profiles (name -> definition)
    #[serde(default)]
    pub profiles: HashMap<String, CustomProfile>,

    /// Shell command to run inside the guest after every successful boot.
    ///
    /// Executed after the VM is up and SSH is ready, before any interactive
    /// `shell` / agent launch. A failure is logged at `WARN` and does not
    /// fail the start — a transient hook failure shouldn't strand the VM.
    ///
    /// Maps to `postStartCommand` from `devcontainer.json`.
    #[serde(default)]
    pub post_start: Option<String>,

    /// Default host:guest port forwards applied to every VM startup.
    ///
    /// CLI `--forward-port` values are appended; later entries override
    /// earlier ones with the same guest port.
    #[serde(default)]
    pub forward_ports: Vec<PortForward>,

    /// Self-update behaviour
    #[serde(default)]
    pub updates: crate::update::UpdateConfig,
}

/// User-defined profile in `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomProfile {
    /// Apt packages to install
    #[serde(default)]
    pub apt_packages: Vec<String>,
    /// Shell script to run before apt-get install (e.g. add repos)
    pub pre_install: Option<String>,
    /// Shell script to run after apt-get install
    pub post_install: Option<String>,
    /// Plugin marketplace sources (URL, path, or GitHub repo)
    #[serde(default)]
    pub marketplaces: Vec<String>,
    /// Plugins to install from marketplaces
    #[serde(default)]
    pub plugins: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmConfig {
    /// Number of vCPUs
    #[serde(default = "default_vcpus")]
    pub vcpu_count: NonZeroU8,

    /// Memory size in MiB. Typed [`VmMemory`], so the [`MIN_MEM_MIB`]
    /// floor holds by construction — including on `config.toml` load.
    #[serde(default = "default_mem_mib")]
    pub mem_size_mib: VmMemory,

    /// Path to vmlinux kernel image
    #[serde(default = "default_kernel_path")]
    pub kernel_path: ConfigPath,

    /// Kernel boot arguments
    #[serde(default = "default_boot_args")]
    pub boot_args: String,

    /// Template rootfs size in GiB (used during setup)
    #[serde(default = "default_template_size_gib")]
    pub template_size_gib: GiB,
}

/// CIDR prefix length in `0..=32`. Display formats as `/N` so it can be
/// concatenated directly with an IPv4 address to form a CIDR block.
///
/// Deserialization accepts `"/24"`, `"24"`, or the bare integer `24`.
/// Out-of-range or non-numeric values are rejected at parse time, so no
/// late validation pass is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubnetMask(u8);

impl SubnetMask {
    /// Create from a runtime value. Returns `None` if `bits > 32`.
    pub fn new(bits: u8) -> Option<Self> {
        (bits <= 32).then_some(Self(bits))
    }
}

impl fmt::Display for SubnetMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}", self.0)
    }
}

impl std::str::FromStr for SubnetMask {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digits = s.strip_prefix('/').unwrap_or(s);
        let bits: u8 = digits
            .parse()
            .map_err(|_| format!("'{s}' is not valid CIDR (expected /0../32)"))?;
        Self::new(bits).ok_or_else(|| format!("'{s}' is not valid CIDR (expected /0../32)"))
    }
}

impl Serialize for SubnetMask {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SubnetMask {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SubnetMaskVisitor;

        impl serde::de::Visitor<'_> for SubnetMaskVisitor {
            type Value = SubnetMask;

            #[mutants::skip] // equivalent: serde Visitor::expecting is only used in error messages, not asserted
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a CIDR prefix length: \"/24\", \"24\", or integer 24")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<SubnetMask, E> {
                v.parse().map_err(E::custom)
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<SubnetMask, E> {
                u8::try_from(v)
                    .ok()
                    .and_then(SubnetMask::new)
                    .ok_or_else(|| E::custom(format!("{v} is not valid CIDR (expected 0..=32)")))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<SubnetMask, E> {
                u8::try_from(v)
                    .ok()
                    .and_then(SubnetMask::new)
                    .ok_or_else(|| E::custom(format!("{v} is not valid CIDR (expected 0..=32)")))
            }
        }

        deserializer.deserialize_any(SubnetMaskVisitor)
    }
}

/// Linux network interface name (e.g. `eth0`, `ens5`).
///
/// Construction enforces the kernel's `dev_valid_name` rules: non-empty,
/// not `.` or `..`, no `/` or whitespace, and at most `IFNAMSIZ - 1 = 15`
/// bytes. The constructor is the only entry point, so downstream code
/// holding an `InterfaceName` can use it without re-validating.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InterfaceName(String);

/// Maximum interface name length excluding the trailing NUL.
/// Matches the kernel `IFNAMSIZ - 1`.
const MAX_INTERFACE_NAME_LEN: usize = 15;

impl InterfaceName {
    pub fn new(name: &str) -> Result<Self> {
        validate_interface_name(name)?;
        Ok(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_interface_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Interface name is empty");
    }
    if name == "." || name == ".." {
        bail!("Interface name '{name}' is reserved");
    }
    if name.len() > MAX_INTERFACE_NAME_LEN {
        bail!(
            "Interface name '{name}' too long ({} bytes, max {MAX_INTERFACE_NAME_LEN})",
            name.len()
        );
    }
    validate_safe_chars(name, "Interface name")?;
    Ok(())
}

impl fmt::Display for InterfaceName {
    #[mutants::skip] // equivalent: trivial forwarder; as_str() coverage suffices
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Host interface selection for NAT. Either the [`AUTO_SENTINEL`] literal
/// (auto-detect the default route's interface at runtime) or an explicit
/// [`InterfaceName`].
///
/// A custom serde impl rejects sentinel typos like `"Auto"` or `" auto"` —
/// they fail validation as interface names rather than silently bypassing
/// auto-detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostInterface {
    Auto,
    Named(InterfaceName),
}

/// The string spelling of [`HostInterface::Auto`] in TOML / serde.
const AUTO_SENTINEL: &str = "auto";

impl HostInterface {
    /// String form used in TOML ([`AUTO_SENTINEL`] or the interface name).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Auto => AUTO_SENTINEL,
            Self::Named(name) => name.as_str(),
        }
    }
}

impl fmt::Display for HostInterface {
    #[mutants::skip] // equivalent: trivial forwarder; as_str() coverage suffices
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for HostInterface {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for HostInterface {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == AUTO_SENTINEL {
            return Ok(Self::Auto);
        }
        // Catch sentinel typos that an InterfaceName check wouldn't:
        // "Auto" / "AUTO" pass the charset, " auto" doesn't, but both
        // would silently mask the user's intent.
        if s.eq_ignore_ascii_case(AUTO_SENTINEL) || s.trim() == AUTO_SENTINEL {
            return Err(serde::de::Error::custom(format!(
                "host_iface '{s}' looks like the '{AUTO_SENTINEL}' sentinel — \
                 write it exactly as \"{AUTO_SENTINEL}\" for auto-detection"
            )));
        }
        InterfaceName::new(&s)
            .map(Self::Named)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Host IP on TAP interfaces
    #[serde(default = "default_host_ip")]
    pub host_ip: Ipv4Addr,

    /// Subnet mask in CIDR notation
    #[serde(default = "default_subnet_mask")]
    pub subnet_mask: SubnetMask,

    /// Host network interface for NAT (`"auto"` or a name like `eth0`, `ens5`)
    #[serde(default = "default_host_iface")]
    pub host_iface: HostInterface,
}

/// GitHub authentication mode for the guest VM.
///
/// Accepts either a plain string (`"auto"`, `"env"`, `"off"`, `"pat"`)
/// or a table form. The table form is required to attach per-repo PAT
/// entries (see [`PatConfig`]):
///
/// ```toml
/// [github]
/// mode = "pat"
/// skip = ["owner/big-repo"]
///
/// [github.pat."owner/repo"]
/// token = "cmd:security find-generic-password -s coop-github-pat -a owner-repo -w"
/// ```
///
/// The plain string `"pat"` form is equivalent to a table with `mode = "pat"`
/// and no `pat` entries — lookup will fail at start-time until an entry is added.
#[derive(Debug, Clone)]
pub enum GitHubAuth {
    Auto,
    Env,
    Off,
    Pat(PatConfig),
}

impl GitHubAuth {
    /// Short, human-readable name for the configured mode.
    #[mutants::skip] // equivalent: labels appear only in user-facing log lines that no test asserts against
    pub fn mode_name(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Env => "env",
            Self::Off => "off",
            Self::Pat(_) => "pat",
        }
    }

    /// Look up a `[github.pat."owner/repo"]` entry by repo slug.
    ///
    /// Returns `None` for non-pat modes or when no entry exists.
    pub fn pat_entry(&self, repo: &crate::github_repo::RepoSlug) -> Option<&PatEntry> {
        match self {
            Self::Pat(cfg) => cfg.entries.get(repo),
            _ => None,
        }
    }
}

/// Top-level `[github]` table with PAT entries and skip markers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatConfig {
    /// Per-repo PAT entries, keyed by `owner/repo`.
    ///
    /// Uses `BTreeMap` so TOML serialization is stable across runs.
    /// Invalid slugs are rejected at deserialization time by
    /// [`RepoSlug`](crate::github_repo::RepoSlug).
    #[serde(default, rename = "pat")]
    pub entries: std::collections::BTreeMap<crate::github_repo::RepoSlug, PatEntry>,
    /// Repos for which the VM startup auto-prompt is suppressed.
    #[serde(default)]
    pub skip: Vec<crate::github_repo::RepoSlug>,
}

/// One per-repo PAT entry under `[github.pat."owner/repo"]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatEntry {
    /// Token value. Accepts a literal token or a `cmd:`-prefixed shell
    /// command. Resolved via [`resolve_cmd_value`].
    pub token: Secret<String>,
}

/// Custom deserializer accepts either a string (legacy/simple) or a table.
impl<'de> Deserialize<'de> for GitHubAuth {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, Visitor};

        struct AuthVisitor;

        impl<'de> Visitor<'de> for AuthVisitor {
            type Value = GitHubAuth;

            #[mutants::skip] // equivalent: serde Visitor::expecting is only used in error messages, not asserted
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string (\"auto\" / \"env\" / \"off\" / \"pat\") or a table")
            }

            fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "auto" => Ok(GitHubAuth::Auto),
                    "env" => Ok(GitHubAuth::Env),
                    "off" => Ok(GitHubAuth::Off),
                    "pat" => Ok(GitHubAuth::Pat(PatConfig::default())),
                    other => Err(E::custom(format!(
                        "unknown github mode '{other}' (expected auto, env, off, or pat)"
                    ))),
                }
            }

            fn visit_string<E: Error>(self, v: String) -> Result<Self::Value, E> {
                self.visit_str(&v)
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                #[derive(Deserialize)]
                #[serde(field_identifier, rename_all = "snake_case")]
                enum Field {
                    Mode,
                    Pat,
                    Skip,
                    #[serde(other)]
                    Unknown,
                }

                let mut mode: Option<String> = None;
                let mut entries: Option<
                    std::collections::BTreeMap<crate::github_repo::RepoSlug, PatEntry>,
                > = None;
                let mut skip: Option<Vec<crate::github_repo::RepoSlug>> = None;
                while let Some(field) = map.next_key::<Field>()? {
                    match field {
                        Field::Mode => mode = Some(map.next_value()?),
                        Field::Pat => entries = Some(map.next_value()?),
                        Field::Skip => skip = Some(map.next_value()?),
                        Field::Unknown => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                // When the table form omits `mode`, the implied mode is
                // "pat" if any pat/skip entries are present (the user is
                // recording per-repo intent without explicitly stating
                // the mode); otherwise "off".
                let has_pat_data =
                    entries.as_ref().is_some_and(|m| !m.is_empty()) || skip.is_some();
                let mode = mode
                    .or_else(|| has_pat_data.then(|| "pat".to_string()))
                    .unwrap_or_else(|| "off".to_string());
                match mode.as_str() {
                    "auto" => Ok(GitHubAuth::Auto),
                    "env" => Ok(GitHubAuth::Env),
                    "off" => Ok(GitHubAuth::Off),
                    "pat" => Ok(GitHubAuth::Pat(PatConfig {
                        entries: entries.unwrap_or_default(),
                        skip: skip.unwrap_or_default(),
                    })),
                    other => Err(M::Error::custom(format!(
                        "unknown github mode '{other}' (expected auto, env, off, or pat)"
                    ))),
                }
            }
        }

        deserializer.deserialize_any(AuthVisitor)
    }
}

/// Serialize as a string for `auto`/`env`/`off` modes, and as a table
/// for `pat` mode (so per-repo entries round-trip).
impl Serialize for GitHubAuth {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Env => serializer.serialize_str("env"),
            Self::Off => serializer.serialize_str("off"),
            Self::Pat(cfg) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("mode", "pat")?;
                if !cfg.entries.is_empty() {
                    map.serialize_entry("pat", &cfg.entries)?;
                }
                if !cfg.skip.is_empty() {
                    map.serialize_entry("skip", &cfg.skip)?;
                }
                map.end()
            }
        }
    }
}

/// Definition of an MCP server registered in the guest. The variant
/// selects the transport — `Stdio` spawns a local process, `Http` and
/// `Sse` connect to a remote URL. Each variant carries only the fields
/// that are meaningful for that transport, so unrepresentable combinations
/// (e.g. `command` + `url`) cannot be constructed or deserialized.
#[derive(Debug, Clone)]
pub enum McpServerDef {
    /// Local server launched via stdin/stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        /// Env var name mappings: key is the variable the server reads,
        /// value is the host env var name whose contents to forward.
        /// Both sides are validated POSIX names at deserialize time.
        env: BTreeMap<crate::guest_env_state::EnvVarName, crate::guest_env_state::EnvVarName>,
    },
    /// Remote server reached over HTTP.
    Http {
        url: url::Url,
        /// Header names are not secret, but values may carry tokens
        /// (e.g. `Authorization`), so they are wrapped in [`Secret`].
        headers: HashMap<String, Secret<String>>,
    },
    /// Remote server reached over Server-Sent Events.
    Sse {
        url: url::Url,
        /// See [`McpServerDef::Http`] for why values are [`Secret`].
        headers: HashMap<String, Secret<String>>,
    },
}

impl McpServerDef {
    /// Resolve any `cmd:`-prefixed header values via [`resolve_cmd_value`].
    /// Stdio servers carry no secret-bearing fields, so they are returned unchanged.
    pub(crate) fn resolve_header_secrets(&mut self, label: &str, name: &str) -> Result<()> {
        let headers = match self {
            McpServerDef::Stdio { .. } => return Ok(()),
            McpServerDef::Http { headers, .. } | McpServerDef::Sse { headers, .. } => headers,
        };
        for (key, value) in headers {
            let resolved = resolve_cmd_value(value.expose()).with_context(|| {
                format!("Failed to resolve header '{key}' for {label} '{name}'")
            })?;
            *value = Secret::new(resolved);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for McpServerDef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;

        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            command: Option<String>,
            #[serde(default)]
            args: Vec<String>,
            #[serde(rename = "type", default)]
            transport: Option<String>,
            #[serde(default)]
            url: Option<String>,
            #[serde(default)]
            env: BTreeMap<crate::guest_env_state::EnvVarName, crate::guest_env_state::EnvVarName>,
            #[serde(default)]
            headers: HashMap<String, Secret<String>>,
        }

        let Raw {
            command,
            args,
            transport,
            url,
            env,
            headers,
        } = Raw::deserialize(deserializer)?;

        match transport.as_deref() {
            None | Some("stdio") => {
                if let Some(url) = url {
                    return Err(D::Error::custom(format!(
                        "stdio MCP server must not have a `url` field (got '{url}')"
                    )));
                }
                if !headers.is_empty() {
                    return Err(D::Error::custom(
                        "stdio MCP server must not have a `headers` field",
                    ));
                }
                let command = command.ok_or_else(|| D::Error::missing_field("command"))?;
                Ok(McpServerDef::Stdio { command, args, env })
            }
            Some(kind @ ("http" | "sse")) => {
                if command.is_some() {
                    return Err(D::Error::custom(format!(
                        "{kind} MCP server must not have a `command` field"
                    )));
                }
                if !args.is_empty() {
                    return Err(D::Error::custom(format!(
                        "{kind} MCP server must not have an `args` field"
                    )));
                }
                if !env.is_empty() {
                    return Err(D::Error::custom(format!(
                        "{kind} MCP server must not have an `env` field"
                    )));
                }
                let url_str = url.ok_or_else(|| D::Error::missing_field("url"))?;
                let url = url::Url::parse(&url_str)
                    .map_err(|e| D::Error::custom(format!("invalid url '{url_str}': {e}")))?;
                Ok(match kind {
                    "http" => McpServerDef::Http { url, headers },
                    _ => McpServerDef::Sse { url, headers },
                })
            }
            Some(other) => Err(D::Error::custom(format!(
                "unknown MCP server type '{other}' (expected 'stdio', 'http', or 'sse')"
            ))),
        }
    }
}

/// Field count passed to `serialize_map` as a size hint for a stdio server.
///
/// The hint is advisory: `serde_json` and toml ignore it, so its arithmetic has
/// no observable effect. Isolating it here keeps the field-emission logic in
/// [`McpServerDef::serialize`] — which the `mcp_server_serializes_*` tests
/// pin — separately mutation-tested.
#[mutants::skip] // equivalent: serialize_map size hint is ignored by serde_json/toml
fn stdio_map_len(args: &[String], env: &BTreeMap<EnvVarName, EnvVarName>) -> usize {
    1 + usize::from(!args.is_empty()) + usize::from(!env.is_empty())
}

/// Field count passed to `serialize_map` as a size hint for an http/sse server.
///
/// As with [`stdio_map_len`], the hint is advisory and ignored by the
/// self-describing formats coop emits, so its arithmetic is unobservable.
#[mutants::skip] // equivalent: serialize_map size hint is ignored by serde_json/toml
fn remote_map_len(headers: &HashMap<String, Secret<String>>) -> usize {
    2 + usize::from(!headers.is_empty())
}

impl Serialize for McpServerDef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        match self {
            McpServerDef::Stdio { command, args, env } => {
                let len = stdio_map_len(args, env);
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("command", command)?;
                if !args.is_empty() {
                    map.serialize_entry("args", args)?;
                }
                if !env.is_empty() {
                    map.serialize_entry("env", env)?;
                }
                map.end()
            }
            McpServerDef::Http { url, headers } => {
                serialize_remote(serializer, "http", url, headers)
            }
            McpServerDef::Sse { url, headers } => serialize_remote(serializer, "sse", url, headers),
        }
    }
}

fn serialize_remote<S: serde::Serializer>(
    serializer: S,
    kind: &'static str,
    url: &url::Url,
    headers: &HashMap<String, Secret<String>>,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;

    let len = remote_map_len(headers);
    let mut map = serializer.serialize_map(Some(len))?;
    map.serialize_entry("type", kind)?;
    map.serialize_entry("url", url)?;
    if !headers.is_empty() {
        map.serialize_entry("headers", headers)?;
    }
    map.end()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConfigDir {
    #[default]
    Default,
    Custom(ConfigPath),
    Disabled,
}

impl serde::Serialize for ConfigDir {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Default => serializer.serialize_none(),
            Self::Custom(path) => serializer.serialize_str(&path.to_string_lossy()),
            Self::Disabled => serializer.serialize_bool(false),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ConfigDir {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ConfigDirVisitor;

        impl serde::de::Visitor<'_> for ConfigDirVisitor {
            type Value = ConfigDir;

            #[mutants::skip] // equivalent: serde Visitor::expecting is only used in error messages, not asserted
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a path string, false, or null")
            }

            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                if v {
                    Err(E::custom(
                        "config_dir does not accept true — use a path string or false",
                    ))
                } else {
                    Ok(ConfigDir::Disabled)
                }
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(ConfigDir::Custom(ConfigPath::new(v)))
            }

            #[mutants::skip] // equivalent: serde routes owned strings through visit_str; this path isn't exercised by our deserializer
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(ConfigDir::Custom(ConfigPath::new(v)))
            }

            #[mutants::skip] // equivalent: only called by serde for input shapes we don't accept
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(ConfigDir::Default)
            }

            #[mutants::skip] // equivalent: only called by serde for input shapes we don't accept
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(ConfigDir::Default)
            }
        }

        deserializer.deserialize_any(ConfigDirVisitor)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeConfig {
    /// Anthropic API key (forwarded via `SendEnv`, never written to disk)
    pub api_key: Option<Secret<String>>,

    /// Additional env var names to forward from host to guest via SSH
    #[serde(default)]
    pub env_forward: Vec<EnvVarName>,

    /// Plugin marketplace sources (URL, path, or GitHub repo)
    #[serde(default)]
    pub marketplaces: Vec<String>,

    /// Plugins to install from marketplaces
    #[serde(default)]
    pub plugins: Vec<String>,

    /// MCP servers to register (name -> definition)
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerDef>,

    /// Source directory for Claude config files (CLAUDE.md, rules/, commands/)
    #[serde(default)]
    pub config_dir: ConfigDir,

    /// Local-model endpoint to route Claude Code at when this VM is
    /// switched to local mode (see `coop model`). Takes precedence over
    /// any endpoint prompted interactively and saved in instance state.
    #[serde(default)]
    pub local_model: Option<LocalModel>,
}

/// `[setup]` section: controls one-time UX behaviour at VM startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupConfig {
    /// Whether VM startup prompts the user to set up a fine-grained PAT
    /// when the resolved repo has no entry. Set to `false` to suppress
    /// globally. The per-repo `skip` list under `[github]` suppresses
    /// individual repos.
    #[serde(default = "default_prompt_for_pat")]
    pub prompt_for_pat: bool,
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            prompt_for_pat: default_prompt_for_pat(),
        }
    }
}

fn default_prompt_for_pat() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CodexConfig {
    /// `OpenAI` API key (forwarded via `SendEnv`, never written to disk)
    pub api_key: Option<Secret<String>>,

    /// Additional env var names to forward from host to guest via SSH
    #[serde(default)]
    pub env_forward: Vec<EnvVarName>,

    /// Plugin marketplace sources (URL, path, or GitHub repo)
    #[serde(default)]
    pub marketplaces: Vec<String>,

    /// Plugins to install from marketplaces
    #[serde(default)]
    pub plugins: Vec<String>,

    /// MCP servers to register in `~/.codex/config.toml`
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerDef>,

    /// Source directory for Codex config files (config.toml, AGENTS.md, prompts/)
    #[serde(default)]
    pub config_dir: ConfigDir,

    /// Local-model endpoint to route Codex at when this VM is switched to
    /// local mode (see `coop model`). Takes precedence over any endpoint
    /// prompted interactively and saved in instance state.
    #[serde(default)]
    pub local_model: Option<LocalModel>,
}

/// A local (host-side) model endpoint that `coop model <vm> local`
/// materializes into guest agent config.
///
/// `host_url` is the endpoint as seen **on the host** (where the model
/// server runs — Ollama / LM Studio / vLLM / llama.cpp). A local model
/// cannot run inside the headless guest, so coop rewrites a
/// `localhost`/`127.0.0.1` host to the backend's guest-visible host
/// address (Firecracker: the TAP gateway; Lima: `host.lima.internal`)
/// and passes any other host through verbatim, so a LAN endpoint also
/// works. See [`crate::network::rewrite_host_url`].
///
/// The fields are private and the only constructor ([`LocalModel::new`],
/// which the validating [`Deserialize`] impl also routes through)
/// enforces the invariants — an `http`/`https` URL with a host and a
/// non-empty model — so every `LocalModel` in the program is valid by
/// construction and no later validation pass is needed.
#[derive(Debug, Clone, Serialize)]
pub struct LocalModel {
    host_url: url::Url,
    model: String,
    auth_token: Option<Secret<String>>,
}

impl LocalModel {
    /// Construct an endpoint, enforcing the invariants. The single place
    /// a `LocalModel` is validated; deserialization and the interactive
    /// `coop model` prompt both route through here.
    pub fn new(
        host_url: url::Url,
        model: String,
        auth_token: Option<Secret<String>>,
    ) -> Result<Self> {
        if model.trim().is_empty() {
            bail!("local model 'model' must not be empty");
        }
        if !matches!(host_url.scheme(), "http" | "https") {
            bail!("local model host_url '{host_url}' must use http or https");
        }
        if host_url.host_str().is_none() {
            bail!("local model host_url '{host_url}' has no host");
        }
        Ok(Self {
            host_url,
            model,
            auth_token,
        })
    }

    pub fn host_url(&self) -> &url::Url {
        &self.host_url
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// The configured auth token, or the shared dummy fallback used for
    /// permissive local servers that accept any value.
    pub fn auth_token_or_default(&self) -> String {
        self.auth_token.as_ref().map_or_else(
            || LOCAL_MODEL_AUTH_FALLBACK.to_string(),
            |s| s.expose().clone(),
        )
    }
}

impl<'de> Deserialize<'de> for LocalModel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            host_url: url::Url,
            model: String,
            #[serde(default)]
            auth_token: Option<Secret<String>>,
        }
        let raw = Raw::deserialize(deserializer)?;
        LocalModel::new(raw.host_url, raw.model, raw.auth_token).map_err(serde::de::Error::custom)
    }
}

/// Dummy auth token written for local endpoints with no configured token.
/// Permissive local servers (Ollama, LM Studio, vLLM) ignore the value.
pub const LOCAL_MODEL_AUTH_FALLBACK: &str = "coop-local";

const MAX_INSTANCE_NAME_LEN: usize = 64;

fn validate_instance_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Instance name must not be empty");
    }
    if name.len() > MAX_INSTANCE_NAME_LEN {
        bail!(
            "Instance name too long ({} chars, max {MAX_INSTANCE_NAME_LEN})",
            name.len()
        );
    }
    if let Some(c) = name
        .chars()
        .find(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_'))
    {
        let mut msg = format!(
            "Instance name contains invalid character '{c}' \
             (allowed: a-z, A-Z, 0-9, '-', '_')"
        );
        if looks_like_path(name) {
            msg.push_str(
                ".\nIf you meant to create or reconnect to a project environment, \
                 use `coop up <PATH>`",
            );
        }
        bail!(msg);
    }
    Ok(())
}

/// Whether a rejected instance name looks like the user typed a filesystem
/// path by mistake (e.g. `coop start ~/projects/foo`). Used only to enrich
/// the validation error with a hint toward `coop up <PATH>`.
fn looks_like_path(name: &str) -> bool {
    name.contains('/') || name.starts_with('~')
}

/// Validated instance name. Construction guarantees the name matches
/// `[a-zA-Z0-9_-]{1,64}`, so downstream code (path construction,
/// shell commands, SSH config blocks) can use it without re-checking.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstanceName(String);

impl InstanceName {
    pub fn new(name: &str) -> Result<Self> {
        validate_instance_name(name)?;
        Ok(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstanceName {
    #[mutants::skip] // equivalent: trivial forwarder; a test would duplicate the as_str() coverage above
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for InstanceName {
    #[mutants::skip] // equivalent: trivial forwarder; a test would duplicate the as_str() coverage above
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for InstanceName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl Serialize for InstanceName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InstanceName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

const MAX_IMAGE_NAME_LEN: usize = 64;

fn validate_image_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Image name is empty");
    }
    // Banning leading '.' subsumes both '.' and '..' (the traversal cases)
    // and also keeps stray dotfiles out of the images directory.
    if name.starts_with('.') {
        bail!("Image name '{name}' must not start with '.'");
    }
    if name.len() > MAX_IMAGE_NAME_LEN {
        bail!(
            "Image name too long ({} chars, max {MAX_IMAGE_NAME_LEN})",
            name.len()
        );
    }
    validate_safe_chars(name, "Image name")?;
    Ok(())
}

/// Validated golden-image name. Construction guarantees the name matches
/// `[a-zA-Z0-9_.-]{1,64}` and does not begin with `.`, so downstream code
/// (path construction, lookups) can use it without re-checking and is
/// safe from directory traversal (`.` / `..`) and stray dotfile entries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageName(String);

impl ImageName {
    pub fn new(name: &str) -> Result<Self> {
        validate_image_name(name)?;
        Ok(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageName {
    #[mutants::skip] // equivalent: trivial forwarder; a test would duplicate the as_str() coverage above
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ImageName {
    #[mutants::skip] // equivalent: trivial forwarder; a test would duplicate the as_str() coverage above
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for ImageName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl Serialize for ImageName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ImageName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

/// Acquire an exclusive flock on a `.lock` file inside `dir`.
///
/// Returns the open file handle — the lock is held until dropped.
fn lock_dir(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create directory {}", dir.display()))?;
    let lock_path = dir.join(".lock");
    let file = File::create(&lock_path)
        .with_context(|| format!("Failed to create lock file {}", lock_path.display()))?;
    // SAFETY: flock is safe to call on a valid fd. The File owns the fd
    // and outlives this call. LOCK_EX blocks until the lock is acquired.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        bail!(
            "Failed to acquire lock on {}: {}",
            lock_path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(file)
}

/// Sanitize a directory basename for use as an instance name.
///
/// Replaces characters outside `[a-zA-Z0-9_-]` with `-`.
/// Returns `"workspace"` if the result would be empty.
/// Truncates to 60 characters to leave room for collision suffixes.
fn sanitize_basename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        return "workspace".to_string();
    }
    let max = 60.min(sanitized.len());
    sanitized[..max].to_string()
}

/// Find a unique instance name by appending `-2`, `-3`, etc. on collision.
fn unique_instance_name(base: &str, instances: &[Instance]) -> Result<InstanceName> {
    if !instances.iter().any(|i| i.name.as_str() == base) {
        return InstanceName::new(base);
    }
    for n in 2..=99 {
        let candidate = format!("{base}-{n}");
        if !instances.iter().any(|i| i.name.as_str() == candidate) {
            return InstanceName::new(&candidate);
        }
    }
    bail!("Could not find unique instance name for '{base}'")
}

/// Expand a leading `~` in each marketplace entry that is a path.
///
/// Marketplace entries can be a URL, a GitHub repo slug, or a host
/// path, so only entries beginning with `~` are treated as paths.
fn expand_marketplaces(entries: &mut [String]) {
    for entry in entries {
        if entry.starts_with('~') {
            *entry = crate::shell::expand_tilde(Path::new(entry.as_str()))
                .to_string_lossy()
                .into_owned();
        }
    }
}

/// Push a validation error for each marketplace entry that looks like a
/// local (absolute) path but does not exist. `field` is the config key
/// prefix (e.g. `"claude.marketplaces"`) used in the message.
fn check_local_marketplaces(field: &str, entries: &[String], errors: &mut Vec<String>) {
    for mp in entries {
        let path = Path::new(mp);
        if path.is_absolute() && !path.exists() {
            errors.push(format!(
                "{field} entry '{mp}' looks like a local path but does not exist"
            ));
        }
    }
}

impl CoopConfig {
    /// Default config path: `~/.coop/config.toml`.
    pub fn default_path() -> PathBuf {
        default_data_dir().join("config.toml")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut cfg: Self = if path.exists() {
            let content = std::fs::read_to_string(path).context("Failed to read config file")?;
            if path.extension().is_some_and(|ext| ext == "json") {
                serde_json::from_str(&content).context("Failed to parse JSON config file")?
            } else {
                toml::from_str(&content).context("Failed to parse TOML config file")?
            }
        } else {
            tracing::debug!("No config file found at {}, using defaults", path.display());
            Self::default()
        };
        cfg.expand_user_paths();
        Ok(cfg)
    }

    /// Expand a leading `~` in the marketplace entries that are paths.
    ///
    /// Scalar path fields (`data_dir`, `firecracker_bin`,
    /// `vm.kernel_path`) and the `config_dir` enums are [`ConfigPath`]s,
    /// expanded by construction during deserialization, so they need no
    /// handling here — and a future host-path field must be typed
    /// [`ConfigPath`] (never a bare `PathBuf` or `paths::HostPath`) so the
    /// same holds for it automatically. The expansion invariant
    /// lives at the source because the consumers are generic filesystem /
    /// command sinks shared with derived paths (`data_dir.join(..)`), which
    /// cannot require the type. Marketplace entries stay special-cased:
    /// each is a mixed URL / GitHub slug / host path, so only the
    /// path-shaped ones (a leading `~`) can be expanded.
    fn expand_user_paths(&mut self) {
        expand_marketplaces(&mut self.claude.marketplaces);
        expand_marketplaces(&mut self.codex.marketplaces);
        for profile in self.profiles.values_mut() {
            expand_marketplaces(&mut profile.marketplaces);
        }
    }

    /// Fail fast on config errors and surface warnings via `tracing::warn`.
    ///
    /// Lifecycle commands (`up`/`setup`/`start`/`quickstart`) call this at
    /// the handler boundary so a broken config aborts before any expensive
    /// work (image build, PAT prompt, VM boot). It is *not* the gate: the
    /// authoritative environmental check runs at the boot choke point
    /// ([`crate::backend::boot_preflight`], inside each backend's
    /// `setup`/`create_and_start`/`start_existing`) on the freshest
    /// filesystem state, so a new lifecycle path cannot skip it. Query
    /// commands (`list`/`status`/`logs`) skip this call, so an unrelated
    /// config error (e.g. a stale `claude.config_dir`) can't block them.
    ///
    /// `coop validate` uses [`Self::validate`] directly because it prints
    /// warnings to stdout instead of routing them to tracing.
    pub fn validate_and_warn(&self) -> Result<()> {
        for w in self.validate()? {
            tracing::warn!("{w}");
        }
        Ok(())
    }

    /// Validate config values, returning all problems found.
    ///
    /// Checks numeric bounds, IP/CIDR parsing, and path accessibility.
    /// Returns `Ok(warnings)` where warnings are non-fatal observations,
    /// or `Err` with all fatal validation errors joined.
    pub fn validate(&self) -> Result<Vec<String>> {
        let mut errors: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // VM config value bounds are enforced by construction, not here:
        // `vcpu_count`/`template_size_gib`/`ssh_port` are NonZero types
        // (zero rejected at deserialization) and `mem_size_mib` is
        // `VmMemory` (the 128 MiB floor holds by construction). `validate`
        // is therefore environmental-only — every check below probes
        // filesystem state, which no value invariant can witness.
        //
        // Keep it that way: a new *value* bound belongs in the field's
        // constructor (parse-don't-validate), not in this pass, so it
        // cannot be bypassed by a lifecycle path that mutates config after
        // validation. Only *environmental* checks (path existence) belong
        // here; they are re-run at the boot choke point on fresh state.

        // Path checks
        if let Some(parent) = self.data_dir.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            warnings.push(format!(
                "data_dir parent '{}' does not exist \
                 (will be created on setup)",
                parent.display()
            ));
        }

        if self.template_path().exists() {
            // Template built — check kernel too
            if !self.vm.kernel_path.exists() {
                warnings.push(format!(
                    "kernel_path '{}' does not exist",
                    self.vm.kernel_path.display()
                ));
            }
        }

        if !self.firecracker_bin.exists() && cfg!(target_os = "linux") {
            warnings.push(format!(
                "firecracker_bin '{}' does not exist \
                 (will be installed on setup)",
                self.firecracker_bin.display()
            ));
        }

        if let ConfigDir::Custom(ref path) = self.claude.config_dir
            && !path.is_dir()
        {
            errors.push(format!(
                "claude.config_dir '{}' does not exist or is not a directory",
                path.display()
            ));
        }

        if let ConfigDir::Custom(ref path) = self.codex.config_dir
            && !path.is_dir()
        {
            errors.push(format!(
                "codex.config_dir '{}' does not exist or is not a directory",
                path.display()
            ));
        }

        check_local_marketplaces(
            "claude.marketplaces",
            &self.claude.marketplaces,
            &mut errors,
        );
        check_local_marketplaces("codex.marketplaces", &self.codex.marketplaces, &mut errors);

        // `[claude.local_model]` / `[codex.local_model]` invariants
        // (http(s) scheme, present host, non-empty model) are enforced by
        // `LocalModel`'s deserializer, so an invalid endpoint fails at
        // config-load time and never reaches here.

        // `[github.pat]` keys and `github.skip` entries are typed as
        // `RepoSlug`; invalid values are rejected at config load time, so
        // no per-key check is needed here.

        if errors.is_empty() {
            Ok(warnings)
        } else {
            bail!("Config validation failed:\n  - {}", errors.join("\n  - "));
        }
    }

    /// Directory containing all named images.
    pub fn images_dir(&self) -> PathBuf {
        self.data_dir.join("images")
    }

    /// Directory for a specific named image.
    pub fn image_dir(&self, name: &ImageName) -> PathBuf {
        self.images_dir().join(name.as_str())
    }

    /// Path to the template rootfs image for a named image.
    pub fn template_path_for(&self, image: &ImageName) -> PathBuf {
        self.image_dir(image).join("rootfs-template.ext4")
    }

    /// Path to the template config for a named image.
    pub fn template_config_path_for(&self, image: &ImageName) -> PathBuf {
        self.image_dir(image).join("template-config.json")
    }

    /// Path to the Lima base image for a named image.
    pub fn lima_base_path(&self, image: &ImageName) -> PathBuf {
        self.image_dir(image).join("lima-base.img")
    }

    /// Path to the Lima start template for a named image.
    pub fn lima_template_path(&self, image: &ImageName) -> PathBuf {
        self.image_dir(image).join("lima-template.yaml")
    }

    /// Path to the default template rootfs image (shorthand).
    pub fn template_path(&self) -> PathBuf {
        self.template_path_for(&default_image_name())
    }

    /// List all available images with their metadata.
    ///
    /// Directories whose names don't pass [`ImageName`] validation are
    /// skipped (with a tracing warning), so a stray dotfile or hand-edited
    /// entry can't poison the result.
    pub fn list_images(&self) -> Result<Vec<ImageInfo>> {
        let dir = self.images_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut images = Vec::new();
        for entry in fs::read_dir(&dir).context("Failed to read images directory")? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let raw = entry.file_name().to_string_lossy().into_owned();
            let name = match ImageName::new(&raw) {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("Skipping invalid image dir '{raw}': {e}");
                    continue;
                }
            };
            let config_path = self.template_config_path_for(&name);
            let config = if config_path.exists() {
                let content = fs::read_to_string(&config_path).ok();
                content.and_then(|c| serde_json::from_str(&c).ok())
            } else {
                None
            };
            images.push(ImageInfo {
                name,
                dir: entry.path(),
                config,
            });
        }
        images.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        Ok(images)
    }

    /// Path to the SSH private key for guest access
    pub fn ssh_key_path(&self) -> PathBuf {
        self.data_dir.join("vm_key")
    }

    /// Directory containing all instances
    pub fn instances_dir(&self) -> PathBuf {
        self.data_dir.join("instances")
    }

    /// Path to per-project devcontainer discovery preferences.
    #[mutants::skip] // equivalent: default-path getter; no caller asserts the returned PathBuf
    pub fn devcontainer_preferences_path(&self) -> PathBuf {
        self.data_dir.join("devcontainer_preferences.json")
    }

    /// List all existing instances, sorted by index.
    pub fn list_instances(&self) -> Result<Vec<Instance>> {
        let dir = self.instances_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut instances = Vec::new();
        for entry in fs::read_dir(&dir).context("Failed to read instances directory")? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            match Instance::load(&entry.path()) {
                Ok(inst) => instances.push(inst),
                Err(e) => {
                    // Instance dir exists but has missing or corrupted
                    // instance.json — leftover from a crashed start.
                    // Log and skip so callers aren't blocked.
                    tracing::warn!(
                        "Skipping corrupted instance dir {} ({}). \
                         Remove it manually or run `destroy --all`.",
                        entry.path().display(),
                        e,
                    );
                }
            }
        }
        instances.sort_by_key(|i| i.index);
        Ok(instances)
    }

    /// Resolve an instance by name, or auto-select if only one exists.
    pub fn resolve_instance(&self, name: Option<&InstanceName>) -> Result<Instance> {
        if let Some(name) = name {
            // Fast path: `allocate_instance` writes each instance to
            // `instances_dir/<name>/`, so the metadata is one file read
            // away. Skipping the full directory walk avoids parsing every
            // other `instance.json` on `coop stop`/`shell`/`destroy`.
            if let Ok(inst) = Instance::load(&self.instances_dir().join(name.as_str()))
                && &inst.name == name
            {
                return Ok(inst);
            }
            // Miss or stale metadata: fall back to the full listing so
            // the error message still names the available instances.
            let instances = self.list_instances()?;
            return instances
                .into_iter()
                .find(|i| &i.name == name)
                .with_context(|| {
                    let available = self.format_instance_list_or_none();
                    format!(
                        "No instance named '{name}'. {available}\n\
                         Create one with: coop up . --name {name}"
                    )
                });
        }
        let instances = self.list_instances()?;
        if instances.len() == 1 {
            // Safe: we just checked len == 1
            instances
                .into_iter()
                .next()
                .context("Instance list unexpectedly empty")
        } else if instances.is_empty() {
            bail!(
                "No instances found.\n\
                 Create one with: coop up\n\
                 (Run `coop setup` first if you haven't built an image yet.)"
            )
        } else {
            let names: Vec<_> = instances.iter().map(|i| i.name.as_str()).collect();
            bail!(
                "Multiple instances exist. Specify one: {}",
                names.join(", ")
            )
        }
    }

    /// Allocate a new instance.
    ///
    /// Index allocation: starts from highest existing + 1, wrapping around
    /// to fill gaps at the low end when the ceiling is reached.
    /// Valid indices are 0..=252, mapping to guest IPs 172.16.0.2-254.
    /// (172.16.0.0 = network, .1 = host, .255 = broadcast — all unusable.)
    ///
    /// When `workspace_path` is provided and no explicit name is given,
    /// the instance name is derived from the workspace directory basename
    /// (e.g. `/home/user/projects/myapp` → `myapp`). Collisions are
    /// resolved by appending `-2`, `-3`, etc.
    ///
    /// Uses flock on the instances directory to prevent races between
    /// concurrent allocations.
    pub fn allocate_instance(
        &self,
        name: Option<&InstanceName>,
        image: &ImageName,
        workspace_path: Option<&Path>,
    ) -> Result<Instance> {
        let _lock = lock_dir(&self.instances_dir())?;

        let instances = self.list_instances()?;
        let used_indices: HashSet<InstanceIndex> = instances.iter().map(|i| i.index).collect();

        // Start from highest + 1 (skipping if at the ceiling or already used),
        // then fall back to the lowest free index.
        let next_after_highest = instances
            .iter()
            .map(|i| i.index.as_u16())
            .max()
            .and_then(|h| InstanceIndex::new(h + 1))
            .filter(|next| !used_indices.contains(next));

        let index = match next_after_highest {
            Some(idx) => idx,
            None => (0..=InstanceIndex::MAX)
                .filter_map(InstanceIndex::new)
                .find(|idx| !used_indices.contains(idx))
                .context("All 253 instance slots are in use")?,
        };

        let name = if let Some(n) = name {
            n.clone()
        } else if let Some(ws) = workspace_path {
            let basename = ws
                .file_name()
                .unwrap_or(OsStr::new("workspace"))
                .to_string_lossy();
            let base = sanitize_basename(&basename);
            unique_instance_name(&base, &instances)?
        } else {
            let s = index.to_string();
            InstanceName::new(&s).context("BUG: InstanceIndex produced invalid name")?
        };

        if instances.iter().any(|i| i.name == name) {
            bail!("Instance '{name}' already exists");
        }

        let dir = self.instances_dir().join(name.as_str());
        let instance = Instance {
            name,
            index,
            dir,
            image: image.clone(),
        };
        instance.save()?;
        Ok(instance)
    }

    fn format_instance_list_or_none(&self) -> String {
        match self.list_instances() {
            Ok(instances) if instances.is_empty() => "No instances exist.".to_string(),
            Ok(instances) => {
                let names: Vec<_> = instances.iter().map(|i| i.name.as_str()).collect();
                format!("Available: {}", names.join(", "))
            }
            Err(_) => String::new(),
        }
    }
}

impl Default for CoopConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            vm: VmConfig::default(),
            network: NetworkConfig::default(),
            ssh_port: default_ssh_port(),
            firecracker_bin: default_firecracker_bin(),
            github: None,
            setup: SetupConfig::default(),
            claude: ClaudeConfig::default(),
            codex: CodexConfig::default(),
            guest_env: BTreeMap::new(),
            profiles: HashMap::new(),
            post_start: None,
            forward_ports: Vec::new(),
            updates: crate::update::UpdateConfig::default(),
        }
    }
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            vcpu_count: default_vcpus(),
            mem_size_mib: default_mem_mib(),
            kernel_path: default_kernel_path(),
            boot_args: default_boot_args(),
            template_size_gib: default_template_size_gib(),
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            host_ip: default_host_ip(),
            subnet_mask: default_subnet_mask(),
            host_iface: default_host_iface(),
        }
    }
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            api_key: std::env::var("ANTHROPIC_API_KEY").ok().map(Secret::new),
            env_forward: Vec::new(),
            marketplaces: Vec::new(),
            plugins: Vec::new(),
            mcp_servers: HashMap::new(),
            config_dir: ConfigDir::Default,
            local_model: None,
        }
    }
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            api_key: std::env::var("OPENAI_API_KEY").ok().map(Secret::new),
            env_forward: Vec::new(),
            marketplaces: Vec::new(),
            plugins: Vec::new(),
            mcp_servers: HashMap::new(),
            config_dir: ConfigDir::Default,
            local_model: None,
        }
    }
}

// ── Image info ────────────────────────────────────────────────

/// Metadata about a named golden image.
pub struct ImageInfo {
    pub name: ImageName,
    pub dir: PathBuf,
    pub config: Option<crate::setup::TemplateConfig>,
}

// ── Instance ──────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct InstanceMeta {
    name: InstanceName,
    index: InstanceIndex,
    #[serde(default = "default_image_name")]
    image: ImageName,
}

/// Returns the [`ImageName`] for [`DEFAULT_IMAGE`]. Direct field
/// construction (skipping `ImageName::new`) is safe here because the
/// const is pinned by the `default_image_is_valid` test below.
pub(crate) fn default_image_name() -> ImageName {
    ImageName(DEFAULT_IMAGE.to_string())
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub name: InstanceName,
    pub index: InstanceIndex,
    pub dir: PathBuf,
    /// Name of the golden image this instance was created from.
    pub image: ImageName,
}

impl Instance {
    pub fn rootfs_path(&self) -> PathBuf {
        self.dir.join("rootfs.ext4")
    }

    pub fn pid_file_path(&self) -> PathBuf {
        self.dir.join("firecracker.pid")
    }

    pub fn api_socket_path(&self) -> PathBuf {
        self.dir.join("firecracker.socket")
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("firecracker.log")
    }

    pub fn vsock_path(&self) -> PathBuf {
        self.dir.join("vsock.sock")
    }

    pub fn vm_config_path(&self) -> PathBuf {
        self.dir.join("vm_config.json")
    }

    pub fn workspace_state_path(&self) -> PathBuf {
        self.dir.join("workspace.json")
    }

    pub fn forwards_state_path(&self) -> PathBuf {
        self.dir.join("forwards.json")
    }

    pub fn guest_env_state_path(&self) -> PathBuf {
        self.dir.join("guest_env.json")
    }

    pub fn model_state_path(&self) -> PathBuf {
        self.dir.join("model.json")
    }

    #[mutants::skip] // equivalent: default-path getter; no caller asserts the returned PathBuf
    pub fn devcontainer_state_path(&self) -> PathBuf {
        self.dir.join("devcontainer_state.json")
    }

    pub fn tap_device(&self) -> String {
        format!("tap{}", self.index)
    }

    /// The guest's IPv4 address on the `172.16.0.0/24` host network.
    ///
    /// Derived from the validated [`InstanceIndex`] (`0..=252`), so the
    /// last octet is always in `2..=254` and the address never fails to
    /// form — callers receive an [`Ipv4Addr`] directly rather than a
    /// string they have to re-parse.
    pub fn guest_ip(&self) -> std::net::Ipv4Addr {
        let base = u32::from(std::net::Ipv4Addr::new(172, 16, 0, 2));
        std::net::Ipv4Addr::from(base + self.index.as_u32())
    }

    pub fn guest_mac(&self) -> String {
        format!("06:00:AC:10:00:{:02x}", self.index.as_u32() + 2)
    }

    pub fn vsock_cid(&self) -> u32 {
        self.index.as_u32() + 3
    }

    fn meta_path(&self) -> PathBuf {
        self.dir.join("instance.json")
    }

    fn save(&self) -> Result<()> {
        let meta = InstanceMeta {
            name: self.name.clone(),
            index: self.index,
            image: self.image.clone(),
        };
        let json =
            serde_json::to_string_pretty(&meta).context("Failed to serialize instance metadata")?;
        crate::fs_util::atomic_write_json(&self.meta_path(), &json)
            .context("Failed to write instance.json")?;
        Ok(())
    }

    /// Update the recorded origin image and persist `instance.json`.
    ///
    /// Used by `coop restore`: after the disk is replaced with image
    /// `image`'s template, the instance's lineage (and the guest-user
    /// lookup that keys off it) must track the restored image.
    pub(crate) fn set_image(&mut self, image: ImageName) -> Result<()> {
        self.image = image;
        self.save()
    }

    fn load(dir: &Path) -> Result<Self> {
        let meta_path = dir.join("instance.json");
        let content = fs::read_to_string(&meta_path)
            .with_context(|| format!("Failed to read {}", meta_path.display()))?;
        let meta: InstanceMeta =
            serde_json::from_str(&content).context("Failed to parse instance.json")?;
        Ok(Instance {
            name: meta.name,
            index: meta.index,
            dir: dir.to_path_buf(),
            image: meta.image,
        })
    }

    /// Check if this instance has a running Firecracker process.
    ///
    /// Validates the PID is alive and belongs to a Firecracker process
    /// (guards against PID reuse). Removes stale PID files as a side
    /// effect when the process is gone or belongs to something else.
    pub fn is_running(&self) -> bool {
        let pid_path = self.pid_file_path();
        if !pid_path.exists() {
            return false;
        }
        let Ok(pid_str) = fs::read_to_string(&pid_path) else {
            return false;
        };
        let Ok(pid) = pid_str.trim().parse::<u32>() else {
            return false;
        };

        let alive = Cmd::new("kill")
            .args(["-0", &pid.to_string()])
            .sudo()
            .status_ok();

        if !alive {
            tracing::debug!(
                "Removing stale PID file for instance '{}' (PID {pid} not running)",
                self.name
            );
            if let Err(e) = fs::remove_file(&pid_path) {
                tracing::debug!("Failed to remove stale PID file (non-fatal): {e}");
            }
            return false;
        }

        if !is_firecracker_process(pid) {
            tracing::debug!(
                "Removing stale PID file for instance '{}' \
                 (PID {pid} is not a Firecracker process)",
                self.name
            );
            if let Err(e) = fs::remove_file(&pid_path) {
                tracing::debug!("Failed to remove stale PID file (non-fatal): {e}");
            }
            return false;
        }

        true
    }
}

/// Check if a PID belongs to a Firecracker process by reading
/// `/proc/{pid}/cmdline`. Returns `false` if the file is unreadable
/// or the command line does not contain "firecracker".
fn is_firecracker_process(pid: u32) -> bool {
    let Ok(cmdline) = Cmd::new("cat")
        .arg(format!("/proc/{pid}/cmdline"))
        .sudo()
        .capture()
    else {
        return false;
    };
    // /proc/pid/cmdline uses NUL as separator
    cmdline.contains("firecracker")
}

// ── Defaults ──────────────────────────────────────────────────

fn default_data_dir() -> ConfigPath {
    ConfigPath::new(
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".coop"),
    )
}

fn default_vcpus() -> NonZeroU8 {
    #[expect(clippy::expect_used, reason = "literal is provably non-zero")]
    NonZeroU8::new(2).expect("2 is non-zero")
}

fn default_mem_mib() -> VmMemory {
    #[expect(
        clippy::expect_used,
        reason = "literal is provably non-zero and above the 128 MiB floor"
    )]
    VmMemory::new(MiB::new(4096).expect("4096 is non-zero")).expect("4096 MiB is above the floor")
}

fn default_kernel_path() -> ConfigPath {
    ConfigPath::new(default_data_dir().join("vmlinux"))
}

fn default_template_size_gib() -> GiB {
    #[expect(clippy::expect_used, reason = "literal is provably non-zero")]
    GiB::new(8).expect("8 is non-zero")
}

#[mutants::skip] // equivalent: the kernel cmdline only matters when a VM actually boots, which integration tests cover
fn default_boot_args() -> String {
    "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw".to_string()
}

fn default_host_ip() -> Ipv4Addr {
    Ipv4Addr::new(172, 16, 0, 1)
}

fn default_subnet_mask() -> SubnetMask {
    #[expect(clippy::expect_used, reason = "literal 24 is in 0..=32")]
    SubnetMask::new(24).expect("24 is in 0..=32")
}

fn default_host_iface() -> HostInterface {
    HostInterface::Auto
}

fn default_ssh_port() -> NonZeroU16 {
    #[expect(clippy::expect_used, reason = "literal is provably non-zero")]
    NonZeroU16::new(22).expect("22 is non-zero")
}

fn default_firecracker_bin() -> ConfigPath {
    ConfigPath::new(default_data_dir().join("firecracker"))
}

/// Bounded no-panic proofs (Kani), gated so normal `cargo build`/`test`/
/// `clippy` never compile them. Run manually with `cargo kani`; see the
/// "Formal verification" section in `CLAUDE.md`.
///
/// Kani is a narrow fit in a string-heavy CLI — the type system carries
/// most invariants here. These proofs target the only genuine fit:
/// bounded integer/float arithmetic where a wrap or overflow would panic.
/// The `InstanceIndex` IP-derivation range is verified instead by the
/// exhaustive `0..=252` unit test (`instance_network_derivations_over_full_range`),
/// and `SubnetMask` carries no arithmetic to verify — it is stored and
/// rendered as `/N`, with the `0..=32` bound enforced by its constructor.
#[cfg(kani)]
mod proofs {
    use super::{GiB, InstanceIndex, MiB};

    /// The arithmetic kernel of `DiskSize::resolve`'s relative branch:
    /// `current.checked_add(delta)`. Over every pair of non-zero `u32`
    /// sizes it yields `Some(current + delta)` exactly when the sum fits,
    /// and `None` otherwise — it never wraps and never panics.
    ///
    /// This proves the kernel directly rather than through `resolve`,
    /// because `resolve` wraps the `None` case with `anyhow`'s
    /// heap-allocating error construction, which CBMC cannot model
    /// tractably. `resolve` adds only that infallible `.context()` wrapper
    /// on top of this kernel; its end-to-end behavior — including the
    /// overflow → `Err` path — is pinned by the deterministic unit tests
    /// `disk_size_resolve_relative` and `disk_size_resolve_relative_overflows`.
    #[kani::proof]
    fn disk_relative_add_never_wraps() {
        let current_raw: u32 = kani::any();
        let delta_raw: u32 = kani::any();
        let (Some(current), Some(delta)) = (GiB::new(current_raw), GiB::new(delta_raw)) else {
            return;
        };
        match current
            .as_nonzero()
            .checked_add(delta.as_u32())
            .map(GiB::from_nonzero)
        {
            Some(sum) => assert_eq!(sum.as_u32(), current_raw + delta_raw),
            None => assert!(current_raw.checked_add(delta_raw).is_none()),
        }
    }

    /// `MiB::as_gib_f64` divides a non-zero `u32` by 1024.0, so the result
    /// is always finite and strictly positive across the whole range.
    #[kani::proof]
    fn mib_as_gib_f64_is_finite_and_positive() {
        let raw: u32 = kani::any();
        let Some(mib) = MiB::new(raw) else { return };
        let gib = mib.as_gib_f64();
        assert!(gib.is_finite());
        assert!(gib > 0.0);
    }

    /// The guest IP/MAC last octet is `index + 2`. For every valid index
    /// (`0..=252`) that sum stays within `2..=254`, so it never overflows
    /// `u32` and always fits the final IPv4/MAC octet (`u8`).
    #[kani::proof]
    fn instance_index_octet_stays_in_range() {
        let raw: u16 = kani::any();
        let Some(index) = InstanceIndex::new(raw) else {
            return;
        };
        let octet = index.as_u32() + 2;
        assert!((2..=254).contains(&octet));
        assert!(u8::try_from(octet).is_ok());
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
#[expect(clippy::panic, reason = "tests use panic! for unreachable arms")]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> CoopConfig {
        CoopConfig {
            data_dir: ConfigPath::new(tmp.path()),
            ..CoopConfig::default()
        }
    }

    #[test]
    fn validate_unique_guest_paths_rejects_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let mounts = vec![
            Mount::parse(&format!("{}:/data", a.display())).unwrap(),
            Mount::parse(&format!("{}:/data", b.display())).unwrap(),
        ];
        let err = validate_unique_guest_paths(&mounts).unwrap_err();
        assert!(format!("{err}").contains("/data"));
    }

    #[test]
    fn validate_unique_guest_paths_accepts_distinct() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let mounts = vec![
            Mount::parse(&format!("{}:/data", a.display())).unwrap(),
            Mount::parse(&format!("{}:/other", b.display())).unwrap(),
        ];
        validate_unique_guest_paths(&mounts).unwrap();
        // The empty set is trivially conflict-free.
        validate_unique_guest_paths(&[]).unwrap();
    }

    fn default_img() -> ImageName {
        ImageName::new(DEFAULT_IMAGE).unwrap()
    }

    fn idx(n: u16) -> InstanceIndex {
        InstanceIndex::new(n).unwrap()
    }

    fn iname(s: &str) -> InstanceName {
        InstanceName::new(s).unwrap()
    }

    fn make_instance(dir: &Path, name: &str, index: InstanceIndex) -> Instance {
        let inst = Instance {
            name: InstanceName::new(name).unwrap(),
            index,
            dir: dir.join("instances").join(name),
            image: ImageName::new(DEFAULT_IMAGE).unwrap(),
        };
        inst.save().unwrap();
        inst
    }

    fn test_inst(name: &str, index: InstanceIndex, dir: PathBuf) -> Instance {
        Instance {
            name: InstanceName::new(name).unwrap(),
            index,
            dir,
            image: ImageName::new(DEFAULT_IMAGE).unwrap(),
        }
    }

    // ── InstanceIndex constructor / deserialization ──────────

    #[test]
    fn instance_index_accepts_zero_to_max() {
        assert_eq!(InstanceIndex::new(0).unwrap().as_u16(), 0);
        assert_eq!(
            InstanceIndex::new(InstanceIndex::MAX).unwrap().as_u16(),
            InstanceIndex::MAX
        );
    }

    #[test]
    fn instance_index_rejects_above_max() {
        assert!(InstanceIndex::new(InstanceIndex::MAX + 1).is_none());
        assert!(InstanceIndex::new(u16::MAX).is_none());
    }

    #[test]
    fn instance_index_try_from_reports_value() {
        let err = InstanceIndex::try_from(500).unwrap_err();
        assert_eq!(err.0, 500);
        assert!(err.to_string().contains("500"));
        assert!(err.to_string().contains("0..=252"));
    }

    #[test]
    fn instance_index_deserialize_rejects_out_of_range() {
        let err = serde_json::from_str::<InstanceIndex>("253").unwrap_err();
        assert!(err.to_string().contains("253"), "{err}");
    }

    #[test]
    fn instance_load_rejects_out_of_range_index() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("inst");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("instance.json"),
            r#"{"name": "test", "index": 300, "image": "default"}"#,
        )
        .unwrap();
        let err = Instance::load(&dir).unwrap_err();
        let chain = err.chain().fold(String::new(), |mut acc, e| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{e} | ");
            acc
        });
        assert!(
            chain.contains("300") && chain.contains("0..=252"),
            "expected error to mention 300 and 0..=252, got: {chain}"
        );
    }

    // ── Instance network derivation ──────────────────────────

    /// Every derivation is driven by the index. Exhaustively checking all
    /// 253 valid indices (`0..=252`) pins the arithmetic across the full
    /// range with no toolchain cost, so overflow, wrap, or off-by-one bugs
    /// at any point — not just the endpoints — fail the test. This is the
    /// zero-dependency guarantee preferred over a bounded model check for
    /// `InstanceIndex` (see #303).
    #[test]
    fn instance_network_derivations_over_full_range() {
        for n in 0..=InstanceIndex::MAX {
            let inst = test_inst("test", idx(n), PathBuf::from("/tmp/fake"));
            let octet = u8::try_from(n + 2).unwrap();

            assert_eq!(
                inst.guest_ip(),
                std::net::Ipv4Addr::new(172, 16, 0, octet),
                "guest_ip at index {n}"
            );
            assert_eq!(
                inst.guest_mac(),
                format!("06:00:AC:10:00:{octet:02x}"),
                "guest_mac at index {n}"
            );
            assert_eq!(
                inst.tap_device(),
                format!("tap{n}"),
                "tap_device at index {n}"
            );
            assert_eq!(inst.vsock_cid(), u32::from(n) + 3, "vsock_cid at index {n}");
        }
    }

    // ── Instance save/load roundtrip ─────────────────────────

    #[test]
    fn instance_save_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("myinst");
        let inst = test_inst("myinst", idx(42), dir.clone());
        inst.save().unwrap();

        let loaded = Instance::load(&dir).unwrap();
        assert_eq!(loaded.name, *"myinst");
        assert_eq!(loaded.index.as_u16(), 42);
        assert_eq!(loaded.dir, dir);
        assert_eq!(loaded.image.as_str(), DEFAULT_IMAGE);
    }

    #[test]
    fn set_image_updates_field_and_persists() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("myinst");
        let mut inst = test_inst("myinst", idx(7), dir.clone());
        inst.save().unwrap();

        let restored = ImageName::new("safe-point").unwrap();
        inst.set_image(restored.clone()).unwrap();

        // The in-memory instance reflects the new image …
        assert_eq!(inst.image, restored);
        // … and so does the persisted instance.json (so the guest-user
        // lookup and `coop status` lineage survive a reload).
        let loaded = Instance::load(&dir).unwrap();
        assert_eq!(loaded.image.as_str(), "safe-point");
    }

    // ── Instance::is_running / is_firecracker_process ────────
    //
    // These tests drive `is_running` through real PID files and
    // real subprocesses. The happy path requires a process whose
    // `/proc/<pid>/cmdline` contains "firecracker", which we
    // synthesize with `bash -c 'exec -a firecracker-test sleep'`
    // — argv[0] is renamed so the matcher recognizes it.
    //
    // Sudo-using paths (`kill -0`, `cat /proc/.../cmdline`) are
    // gated `#[cfg(target_os = "linux")]`: CI runs Linux with
    // passwordless sudo, and `/proc/<pid>/cmdline` is Linux-only.
    // The non-sudo paths (missing PID file) run cross-platform.

    /// PID likely-but-not-guaranteed dead. If it happens to belong
    /// to a live non-firecracker process, the tests still see the
    /// expected `false` outcome — they assert behavior, not which
    /// branch fired.
    #[cfg(target_os = "linux")]
    const DEAD_PID: u32 = 999_999;

    #[cfg(target_os = "linux")]
    fn spawn_firecracker_like() -> std::process::Child {
        std::process::Command::new("bash")
            .args(["-c", "exec -a firecracker-test sleep 30"])
            .spawn()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn spawn_sleep() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap()
    }

    #[test]
    fn is_running_false_when_pid_file_missing() {
        let tmp = TempDir::new().unwrap();
        let inst = test_inst("test", idx(0), tmp.path().to_path_buf());
        assert!(!inst.pid_file_path().exists());
        assert!(!inst.is_running());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn is_running_false_for_dead_pid_and_removes_pid_file() {
        let tmp = TempDir::new().unwrap();
        let inst = test_inst("test", idx(0), tmp.path().to_path_buf());
        fs::write(inst.pid_file_path(), DEAD_PID.to_string()).unwrap();

        assert!(!inst.is_running());
        assert!(
            !inst.pid_file_path().exists(),
            "stale PID file should be removed"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn is_running_false_for_live_non_firecracker_pid_and_removes_pid_file() {
        let tmp = TempDir::new().unwrap();
        let inst = test_inst("test", idx(0), tmp.path().to_path_buf());
        let mut child = spawn_sleep();
        fs::write(inst.pid_file_path(), child.id().to_string()).unwrap();

        let running = inst.is_running();
        let _ = child.kill();
        let _ = child.wait();

        assert!(!running);
        assert!(
            !inst.pid_file_path().exists(),
            "PID file should be removed when PID is not a firecracker"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn is_running_true_for_live_firecracker_like_pid() {
        let tmp = TempDir::new().unwrap();
        let inst = test_inst("test", idx(0), tmp.path().to_path_buf());
        let mut child = spawn_firecracker_like();
        fs::write(inst.pid_file_path(), child.id().to_string()).unwrap();

        let running = inst.is_running();
        let pid_file_kept = inst.pid_file_path().exists();
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            running,
            "is_running must return true for a live firecracker"
        );
        assert!(
            pid_file_kept,
            "PID file should be preserved while firecracker is running"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn is_firecracker_process_false_for_dead_pid() {
        assert!(!is_firecracker_process(DEAD_PID));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn is_firecracker_process_false_for_live_non_firecracker_pid() {
        let mut child = spawn_sleep();
        let pid = child.id();
        let result = is_firecracker_process(pid);
        let _ = child.kill();
        let _ = child.wait();

        assert!(!result);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn is_firecracker_process_true_for_firecracker_named_pid() {
        let mut child = spawn_firecracker_like();
        let pid = child.id();
        let result = is_firecracker_process(pid);
        let _ = child.kill();
        let _ = child.wait();

        assert!(result);
    }

    // ── Allocate instance ────────────────────────────────────

    #[test]
    fn allocate_first_instance_gets_index_zero() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        let inst = cfg.allocate_instance(None, &default_img(), None).unwrap();
        assert_eq!(inst.index.as_u16(), 0);
        assert_eq!(inst.name, *"0");
    }

    #[test]
    fn allocate_sequential_instances() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let a = cfg.allocate_instance(None, &default_img(), None).unwrap();
        let b = cfg.allocate_instance(None, &default_img(), None).unwrap();
        let c = cfg.allocate_instance(None, &default_img(), None).unwrap();

        assert_eq!(a.index.as_u16(), 0);
        assert_eq!(b.index.as_u16(), 1);
        assert_eq!(c.index.as_u16(), 2);
    }

    #[test]
    fn allocate_with_custom_name() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let inst = cfg
            .allocate_instance(Some(&iname("my-project")), &default_img(), None)
            .unwrap();
        assert_eq!(inst.name, *"my-project");
        assert_eq!(inst.index.as_u16(), 0);
    }

    #[test]
    fn allocate_rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        cfg.allocate_instance(Some(&iname("dupe")), &default_img(), None)
            .unwrap();
        let err = cfg
            .allocate_instance(Some(&iname("dupe")), &default_img(), None)
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn allocate_continues_after_highest() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        // Create instance at index 0, then remove it, then create at 1
        let inst0 = cfg
            .allocate_instance(Some(&iname("a")), &default_img(), None)
            .unwrap();
        assert_eq!(inst0.index.as_u16(), 0);
        let inst1 = cfg
            .allocate_instance(Some(&iname("b")), &default_img(), None)
            .unwrap();
        assert_eq!(inst1.index.as_u16(), 1);

        // Remove instance 0 by deleting its dir
        fs::remove_dir_all(&inst0.dir).unwrap();

        // Next allocation should be index 2 (highest + 1), not 0 (gap)
        let inst2 = cfg
            .allocate_instance(Some(&iname("c")), &default_img(), None)
            .unwrap();
        assert_eq!(inst2.index.as_u16(), 2);
    }

    #[test]
    fn allocate_fills_gap_when_at_ceiling() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        // Create instance at index 252 (max)
        make_instance(tmp.path(), "max", idx(252));

        // Create another at index 0 (gap at low end)
        make_instance(tmp.path(), "zero", idx(0));

        // Remove index 0
        fs::remove_dir_all(tmp.path().join("instances/zero")).unwrap();

        // Next should fill gap at 0 since highest (252) is at ceiling
        let inst = cfg
            .allocate_instance(Some(&iname("fill")), &default_img(), None)
            .unwrap();
        assert_eq!(inst.index.as_u16(), 0);
    }

    // ── Instance name validation ──────────────────────────────

    #[test]
    fn validate_name_accepts_valid() {
        for name in ["foo", "my-project", "test_vm", "Dev-Box_01", "a", "A"] {
            validate_instance_name(name).unwrap();
        }
    }

    #[test]
    fn validate_name_rejects_empty() {
        let err = validate_instance_name("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validate_name_rejects_path_traversal() {
        let err = validate_instance_name("../../../tmp/evil").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn validate_name_rejects_spaces_and_special_chars() {
        for name in ["has space", "semi;colon", "new\nline", "sl/ash", "d.ot"] {
            let err = validate_instance_name(name).unwrap_err();
            assert!(
                err.to_string().contains("invalid character"),
                "expected rejection for {name:?}"
            );
        }
    }

    #[test]
    fn validate_name_path_like_suggests_workspace() {
        for name in [
            "/Users/hbrodin/projects/foo",
            "~/projects/foo",
            "./relative",
            "~",
        ] {
            let err = validate_instance_name(name).unwrap_err().to_string();
            assert!(
                err.contains("invalid character"),
                "expected base rejection for {name:?}, got: {err}"
            );
            assert!(
                err.contains("coop up <PATH>"),
                "expected coop up hint for {name:?}, got: {err}"
            );
        }
    }

    #[test]
    fn validate_name_non_path_omits_workspace_hint() {
        for name in ["has space", "semi;colon", "d.ot"] {
            let err = validate_instance_name(name).unwrap_err().to_string();
            assert!(
                err.contains("invalid character"),
                "expected rejection for {name:?}, got: {err}"
            );
            assert!(
                !err.contains("coop up <PATH>"),
                "did not expect path hint for {name:?}, got: {err}"
            );
        }
    }

    #[test]
    fn validate_name_rejects_too_long() {
        let long = "a".repeat(65);
        let err = validate_instance_name(&long).unwrap_err();
        assert!(err.to_string().contains("too long"));
    }

    #[test]
    fn validate_name_accepts_max_length() {
        let max = "a".repeat(64);
        validate_instance_name(&max).unwrap();
    }

    // ── List instances ───────────────────────────────────────

    #[test]
    fn list_empty_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        let instances = cfg.list_instances().unwrap();
        assert!(instances.is_empty());
    }

    #[test]
    fn list_returns_sorted_by_index() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "high", idx(10));
        make_instance(tmp.path(), "low", idx(2));
        make_instance(tmp.path(), "mid", idx(5));

        let instances = cfg.list_instances().unwrap();
        let indices: Vec<u16> = instances.iter().map(|i| i.index.as_u16()).collect();
        assert_eq!(indices, vec![2, 5, 10]);
    }

    // ── Resolve instance ─────────────────────────────────────

    #[test]
    fn resolve_auto_selects_single() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "only", idx(0));

        let inst = cfg.resolve_instance(None).unwrap();
        assert_eq!(inst.name, *"only");
    }

    #[test]
    fn resolve_errors_when_empty() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let err = cfg.resolve_instance(None).unwrap_err();
        assert!(err.to_string().contains("No instances found"));
    }

    #[test]
    fn resolve_errors_when_ambiguous() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "a", idx(0));
        make_instance(tmp.path(), "b", idx(1));

        let err = cfg.resolve_instance(None).unwrap_err();
        assert!(err.to_string().contains("Multiple instances"));
    }

    #[test]
    fn resolve_by_name() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "alpha", idx(0));
        make_instance(tmp.path(), "beta", idx(1));

        let inst = cfg.resolve_instance(Some(&iname("beta"))).unwrap();
        assert_eq!(inst.name, *"beta");
        assert_eq!(inst.index.as_u16(), 1);
    }

    #[test]
    fn resolve_unknown_name_errors() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "real", idx(0));

        let err = cfg.resolve_instance(Some(&iname("fake"))).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No instance named 'fake'"));
        assert!(msg.contains("Available: real"), "missing hint in: {msg}");
    }

    #[test]
    fn resolve_unknown_name_with_no_instances_lists_none() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let err = cfg.resolve_instance(Some(&iname("ghost"))).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No instance named 'ghost'"));
        assert!(
            msg.contains("No instances exist."),
            "missing hint in: {msg}"
        );
    }

    #[test]
    fn resolve_ignores_stale_fast_path_dir() {
        // The fast path reads `instances_dir/<name>/instance.json` and only
        // returns it when the stored name matches the requested name. Here the
        // directory `wanted` holds an instance whose stored name is `decoy`, so
        // the fast path must reject it and the slow path must find the real
        // `wanted` instance living under a differently-named directory.
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        let instances = tmp.path().join("instances");

        let stale = Instance {
            name: iname("decoy"),
            index: idx(0),
            dir: instances.join("wanted"),
            image: default_img(),
        };
        stale.save().unwrap();

        let real = Instance {
            name: iname("wanted"),
            index: idx(1),
            dir: instances.join("elsewhere"),
            image: default_img(),
        };
        real.save().unwrap();

        let inst = cfg.resolve_instance(Some(&iname("wanted"))).unwrap();
        assert_eq!(inst.name, *"wanted");
        assert_eq!(inst.index.as_u16(), 1);
    }

    #[test]
    fn format_instance_list_or_none_empty() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        assert_eq!(cfg.format_instance_list_or_none(), "No instances exist.");
    }

    #[test]
    fn format_instance_list_or_none_lists_names() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        make_instance(tmp.path(), "alpha", idx(0));
        make_instance(tmp.path(), "beta", idx(1));
        assert_eq!(cfg.format_instance_list_or_none(), "Available: alpha, beta");
    }

    // ── ClaudeConfig deserialization ─────────────────────────

    #[test]
    fn claude_config_all_fields() {
        let json = r#"{
            "api_key": "sk-ant-test",
            "env_forward": ["MYORG_KEY"],
            "marketplaces": ["https://github.com/anthropics/plugins"],
            "plugins": ["context7"],
            "mcp_servers": {
                "sentry": {
                    "type": "http",
                    "url": "https://mcp.sentry.dev/mcp"
                }
            }
        }"#;
        let cfg: ClaudeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.api_key.as_ref().map(|s| s.expose().as_str()),
            Some("sk-ant-test")
        );
        assert_eq!(cfg.env_forward, vec![EnvVarName::new("MYORG_KEY").unwrap()]);
        assert_eq!(cfg.marketplaces.len(), 1);
        assert_eq!(cfg.plugins, vec!["context7"]);
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert!(cfg.mcp_servers.contains_key("sentry"));
    }

    #[test]
    fn claude_config_all_defaults() {
        let json = "{}";
        let cfg: ClaudeConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.api_key.is_none());
        assert!(cfg.env_forward.is_empty());
        assert!(cfg.marketplaces.is_empty());
        assert!(cfg.plugins.is_empty());
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn codex_config_all_fields() {
        let json = r#"{
            "api_key": "sk-openai-test",
            "env_forward": ["MYORG_KEY"],
            "marketplaces": ["https://github.com/trailofbits/codex-plugins"],
            "plugins": ["my-lsp@codex-plugins"],
            "mcp_servers": {
                "sentry": {
                    "type": "http",
                    "url": "https://mcp.sentry.dev/mcp"
                }
            }
        }"#;
        let cfg: CodexConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.api_key.as_ref().map(|s| s.expose().as_str()),
            Some("sk-openai-test")
        );
        assert_eq!(cfg.env_forward, vec![EnvVarName::new("MYORG_KEY").unwrap()]);
        assert_eq!(
            cfg.marketplaces,
            vec!["https://github.com/trailofbits/codex-plugins".to_string()]
        );
        assert_eq!(cfg.plugins, vec!["my-lsp@codex-plugins".to_string()]);
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert!(cfg.mcp_servers.contains_key("sentry"));
    }

    #[test]
    fn codex_config_all_defaults() {
        let json = "{}";
        let cfg: CodexConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.api_key.is_none());
        assert!(cfg.env_forward.is_empty());
        assert!(cfg.marketplaces.is_empty());
        assert!(cfg.plugins.is_empty());
        assert!(cfg.mcp_servers.is_empty());
        assert_eq!(cfg.config_dir, ConfigDir::Default);
        assert!(cfg.local_model.is_none());
    }

    // ── LocalModel ───────────────────────────────────────────

    #[test]
    fn local_model_deserializes_full_form() {
        let toml_str = r#"
host_url = "http://localhost:11434"
model = "qwen2.5-coder:32b"
auth_token = "secret-token"
"#;
        let lm: LocalModel = toml::from_str(toml_str).unwrap();
        assert_eq!(lm.host_url().as_str(), "http://localhost:11434/");
        assert_eq!(lm.model(), "qwen2.5-coder:32b");
        assert_eq!(lm.auth_token_or_default(), "secret-token");
    }

    #[test]
    fn local_model_auth_token_defaults_when_absent() {
        let lm: LocalModel =
            toml::from_str("host_url = \"http://localhost:1234\"\nmodel = \"m\"\n").unwrap();
        assert_eq!(lm.auth_token_or_default(), LOCAL_MODEL_AUTH_FALLBACK);
    }

    #[test]
    fn local_model_rejects_empty_model() {
        let err = toml::from_str::<LocalModel>("host_url = \"http://localhost\"\nmodel = \"\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }

    #[test]
    fn local_model_rejects_non_http_scheme() {
        let err = toml::from_str::<LocalModel>("host_url = \"ftp://localhost\"\nmodel = \"m\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("http or https"), "{err}");
    }

    #[test]
    fn local_model_embeds_in_claude_config() {
        let toml_str = r#"
[local_model]
host_url = "http://localhost:11434"
model = "qwen"
"#;
        let cfg: ClaudeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.local_model.unwrap().model(), "qwen");
    }

    #[test]
    fn local_model_invalid_endpoint_fails_config_load() {
        // The invariant is enforced at deserialization, so a bad endpoint
        // never produces a `CoopConfig` to validate later.
        let toml_str = r#"
[codex.local_model]
host_url = "http://localhost:1234"
model = ""
"#;
        assert!(toml::from_str::<CoopConfig>(toml_str).is_err());
    }

    #[test]
    fn github_auth_deserialization() {
        assert!(matches!(
            serde_json::from_str::<GitHubAuth>(r#""auto""#).unwrap(),
            GitHubAuth::Auto
        ));
        assert!(matches!(
            serde_json::from_str::<GitHubAuth>(r#""env""#).unwrap(),
            GitHubAuth::Env
        ));
        assert!(matches!(
            serde_json::from_str::<GitHubAuth>(r#""off""#).unwrap(),
            GitHubAuth::Off
        ));
        assert!(matches!(
            serde_json::from_str::<GitHubAuth>(r#""pat""#).unwrap(),
            GitHubAuth::Pat(_)
        ));
    }

    #[test]
    fn github_auth_rejects_unknown_mode() {
        let err = serde_json::from_str::<GitHubAuth>(r#""bogus""#).unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn github_auth_table_form_with_entries() {
        let toml_str = r#"
mode = "pat"

[pat."trailofbits/coop"]
token = "cmd:echo x"

[pat."trailofbits/coop-plugins"]
token = "cmd:echo y"
"#;
        let auth: GitHubAuth = toml::from_str(toml_str).unwrap();
        let pat = match auth {
            GitHubAuth::Pat(p) => p,
            other => panic!("expected Pat variant, got {other:?}"),
        };
        assert_eq!(pat.entries.len(), 2);
        let slug = crate::github_repo::RepoSlug::new("trailofbits/coop").unwrap();
        assert_eq!(
            pat.entries.get(&slug).map(|e| e.token.expose().as_str()),
            Some("cmd:echo x")
        );
        assert!(pat.skip.is_empty());
    }

    #[test]
    fn github_auth_table_form_with_entries_only_implies_pat() {
        // No explicit `mode`, just per-repo entries. The implied mode is
        // "pat" because `entries` is non-empty (the `!m.is_empty()` guard
        // in `visit_map` flips this branch on).
        let toml_str = r#"
[pat."a/b"]
token = "cmd:echo x"
"#;
        let auth: GitHubAuth = toml::from_str(toml_str).unwrap();
        let pat = match auth {
            GitHubAuth::Pat(p) => p,
            other => panic!("expected Pat variant, got {other:?}"),
        };
        assert_eq!(pat.entries.len(), 1);
        assert!(pat.skip.is_empty());
    }

    #[test]
    fn github_auth_table_form_with_empty_pat_implies_off() {
        // An explicit but empty `pat` table with no mode and no skip
        // means there is no per-repo intent. The implied mode is "off",
        // not "pat", because `!m.is_empty()` is false for an empty map.
        let toml_str = "pat = {}\n";
        let auth: GitHubAuth = toml::from_str(toml_str).unwrap();
        assert!(matches!(auth, GitHubAuth::Off));
    }

    #[test]
    fn github_auth_table_form_with_skip_only() {
        // No explicit `mode`, no entries, only a skip array. Should
        // parse as pat-mode with empty entries and the recorded skip list.
        let toml_str = r#"
skip = ["a/b"]
"#;
        let auth: GitHubAuth = toml::from_str(toml_str).unwrap();
        let pat = match auth {
            GitHubAuth::Pat(p) => p,
            other => panic!("expected Pat variant, got {other:?}"),
        };
        let ab = crate::github_repo::RepoSlug::new("a/b").unwrap();
        assert_eq!(pat.skip, vec![ab]);
        assert!(pat.entries.is_empty());
    }

    #[test]
    fn github_auth_lookup_returns_entry() {
        let toml_str = r#"
mode = "pat"

[pat."a/b"]
token = "cmd:echo x"
"#;
        let auth: GitHubAuth = toml::from_str(toml_str).unwrap();
        let ab = crate::github_repo::RepoSlug::new("a/b").unwrap();
        let cd = crate::github_repo::RepoSlug::new("c/d").unwrap();
        let entry = auth.pat_entry(&ab).unwrap();
        assert_eq!(entry.token.expose(), "cmd:echo x");
        assert!(auth.pat_entry(&cd).is_none());
    }

    #[test]
    fn github_auth_lookup_returns_none_for_non_pat_modes() {
        let ab = crate::github_repo::RepoSlug::new("a/b").unwrap();
        for auth in [GitHubAuth::Auto, GitHubAuth::Env, GitHubAuth::Off] {
            assert!(auth.pat_entry(&ab).is_none());
        }
    }

    #[test]
    fn github_auth_table_form_rejects_invalid_pat_key() {
        // Invalid slug as a `[github.pat."..."]` key fails at parse time.
        let toml_str = r#"
mode = "pat"

[pat."not-a-slug"]
token = "cmd:echo x"
"#;
        let err = toml::from_str::<GitHubAuth>(toml_str).unwrap_err();
        assert!(
            err.to_string().contains("owner/repo"),
            "expected owner/repo error, got: {err}"
        );
    }

    #[test]
    fn github_auth_table_form_rejects_invalid_skip_entry() {
        let toml_str = r#"
mode = "pat"
skip = ["not-a-slug"]
"#;
        let err = toml::from_str::<GitHubAuth>(toml_str).unwrap_err();
        assert!(
            err.to_string().contains("owner/repo"),
            "expected owner/repo error, got: {err}"
        );
    }

    #[test]
    fn github_auth_serializes_round_trip_for_pat() {
        // pat-mode → table form; deserialize the serialized output and
        // confirm the entries survived.
        let ab = crate::github_repo::RepoSlug::new("a/b").unwrap();
        let cd = crate::github_repo::RepoSlug::new("c/d").unwrap();
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            ab.clone(),
            PatEntry {
                token: Secret::new("cmd:echo x".to_string()),
            },
        );
        let auth = GitHubAuth::Pat(PatConfig {
            entries,
            skip: vec![cd.clone()],
        });
        let serialized = toml::to_string(&auth).unwrap();
        let parsed: GitHubAuth = toml::from_str(&serialized).unwrap();
        let pat = match parsed {
            GitHubAuth::Pat(p) => p,
            other => panic!("expected Pat variant after round-trip, got {other:?}"),
        };
        assert_eq!(pat.skip, vec![cd]);
        assert_eq!(
            pat.entries.get(&ab).map(|e| e.token.expose().as_str()),
            Some("cmd:echo x")
        );
    }

    #[test]
    fn github_auth_serializes_string_form_for_simple_modes() {
        for (auth, want) in [
            (GitHubAuth::Auto, "\"auto\""),
            (GitHubAuth::Env, "\"env\""),
            (GitHubAuth::Off, "\"off\""),
        ] {
            // serde_json gives a JSON-style scalar; sufficient to confirm
            // the serializer chose a string, not an object.
            let json = serde_json::to_string(&auth).unwrap();
            assert_eq!(json, want);
        }
    }

    #[test]
    fn mcp_server_stdio_def() {
        let json = r#"{
            "command": "npx",
            "args": ["-y", "@myorg/mcp-server"],
            "env": {"API_KEY": "MYORG_API_KEY"}
        }"#;
        let def: McpServerDef = serde_json::from_str(json).unwrap();
        match def {
            McpServerDef::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "@myorg/mcp-server"]);
                let key = crate::guest_env_state::EnvVarName::new("API_KEY").unwrap();
                assert_eq!(
                    env.get(&key)
                        .map(crate::guest_env_state::EnvVarName::as_str),
                    Some("MYORG_API_KEY")
                );
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn mcp_server_rejects_invalid_env_var_name() {
        let json = r#"{"command": "x", "env": {"123BAD": "VALUE"}}"#;
        let err = serde_json::from_str::<McpServerDef>(json).unwrap_err();
        assert!(
            err.to_string().contains("123BAD"),
            "error should reference the invalid name: {err}"
        );
    }

    #[test]
    fn mcp_server_rejects_invalid_env_var_value() {
        let json = r#"{"command": "x", "env": {"KEY": "not a var name"}}"#;
        let err = serde_json::from_str::<McpServerDef>(json).unwrap_err();
        assert!(
            err.to_string().contains("not a var name"),
            "error should reference the invalid value: {err}"
        );
    }

    #[test]
    fn mcp_server_stdio_def_explicit_type() {
        let json = r#"{
            "type": "stdio",
            "command": "/usr/bin/my-tool"
        }"#;
        let def: McpServerDef = serde_json::from_str(json).unwrap();
        assert!(
            matches!(def, McpServerDef::Stdio { ref command, .. } if command == "/usr/bin/my-tool")
        );
    }

    #[test]
    fn mcp_server_http_def() {
        let json = r#"{
            "type": "http",
            "url": "https://mcp.sentry.dev/mcp"
        }"#;
        let def: McpServerDef = serde_json::from_str(json).unwrap();
        match def {
            McpServerDef::Http { url, headers } => {
                assert_eq!(url.as_str(), "https://mcp.sentry.dev/mcp");
                assert!(headers.is_empty());
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn mcp_server_sse_def() {
        let json = r#"{
            "type": "sse",
            "url": "https://mcp.example.com/sse",
            "headers": {"Authorization": "Bearer x"}
        }"#;
        let def: McpServerDef = serde_json::from_str(json).unwrap();
        match def {
            McpServerDef::Sse { url, headers } => {
                assert_eq!(url.as_str(), "https://mcp.example.com/sse");
                assert_eq!(
                    headers.get("Authorization").map(|v| v.expose().as_str()),
                    Some("Bearer x")
                );
            }
            other => panic!("expected Sse, got {other:?}"),
        }
    }

    #[test]
    fn mcp_server_rejects_stdio_with_url() {
        let json = r#"{"command": "x", "url": "https://example.com/"}"#;
        let err = serde_json::from_str::<McpServerDef>(json).unwrap_err();
        assert!(
            err.to_string()
                .contains("stdio MCP server must not have a `url` field")
        );
    }

    #[test]
    fn mcp_server_rejects_http_with_command() {
        let json = r#"{"type": "http", "url": "https://x/", "command": "y"}"#;
        let err = serde_json::from_str::<McpServerDef>(json).unwrap_err();
        assert!(
            err.to_string()
                .contains("http MCP server must not have a `command` field")
        );
    }

    #[test]
    fn mcp_server_rejects_unknown_type() {
        let json = r#"{"type": "websocket", "url": "wss://x/"}"#;
        let err = serde_json::from_str::<McpServerDef>(json).unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown MCP server type 'websocket'")
        );
    }

    #[test]
    fn mcp_server_rejects_http_missing_url() {
        let json = r#"{"type": "http"}"#;
        let err = serde_json::from_str::<McpServerDef>(json).unwrap_err();
        assert!(err.to_string().contains("missing field `url`"));
    }

    #[test]
    fn mcp_server_rejects_stdio_missing_command() {
        let json = "{}";
        let err = serde_json::from_str::<McpServerDef>(json).unwrap_err();
        assert!(err.to_string().contains("missing field `command`"));
    }

    #[test]
    fn mcp_server_rejects_invalid_url() {
        let json = r#"{"type": "http", "url": "not a url"}"#;
        let err = serde_json::from_str::<McpServerDef>(json).unwrap_err();
        assert!(err.to_string().contains("invalid url 'not a url'"));
    }

    #[test]
    fn mcp_server_serializes_stdio_without_type_field() {
        let def = McpServerDef::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string()],
            env: BTreeMap::new(),
        };
        let json = serde_json::to_value(&def).unwrap();
        assert_eq!(json["command"], "npx");
        assert_eq!(json["args"], serde_json::json!(["-y"]));
        assert!(json.get("type").is_none());
        assert!(json.get("url").is_none());
        assert!(json.get("env").is_none(), "empty env is omitted: {json}");
    }

    #[test]
    fn mcp_server_serializes_http_with_type_tag() {
        let def = McpServerDef::Http {
            url: url::Url::parse("https://mcp.sentry.dev/mcp").unwrap(),
            headers: HashMap::new(),
        };
        let json = serde_json::to_value(&def).unwrap();
        assert_eq!(json["type"], "http");
        assert_eq!(json["url"], "https://mcp.sentry.dev/mcp");
        assert!(json.get("command").is_none());
        assert!(
            json.get("headers").is_none(),
            "empty headers omitted: {json}"
        );
    }

    #[test]
    fn mcp_server_round_trip_preserves_variant() {
        for json in [
            r#"{"command":"x","args":["a","b"],"env":{"K":"V"}}"#,
            r#"{"type":"http","url":"https://x.example/","headers":{"H":"v"}}"#,
            r#"{"type":"sse","url":"https://x.example/sse"}"#,
        ] {
            let def: McpServerDef = serde_json::from_str(json).unwrap();
            let again = serde_json::to_string(&def).unwrap();
            let def2: McpServerDef = serde_json::from_str(&again).unwrap();
            // Re-serializing should yield identical bytes.
            assert_eq!(serde_json::to_string(&def2).unwrap(), again);
        }
    }

    // ── Config loading ────────────────────────────────────────

    #[test]
    fn load_missing_file_returns_defaults() {
        let cfg = CoopConfig::load(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(cfg.vm.vcpu_count.get(), 2);
        assert_eq!(cfg.vm.mem_size_mib.get(), MiB::new(4096).unwrap());
        assert_eq!(cfg.ssh_port.get(), 22);
        assert_eq!(cfg.network.host_ip, Ipv4Addr::new(172, 16, 0, 1));
        assert_eq!(cfg.network.subnet_mask, SubnetMask::new(24).unwrap());
        assert_eq!(cfg.vm.template_size_gib, GiB::new(8).unwrap());
    }

    // ── NonZero type enforcement ─────────────────────────────

    #[test]
    fn mib_rejects_zero() {
        assert!(MiB::new(0).is_none());
        assert!(MiB::new(1).is_some());
    }

    #[test]
    fn mib_as_gib_f64_converts_known_values() {
        // Pin both the divisor (1024) and the operator (/) — three
        // concrete points are enough to fail any constant-return,
        // multiplication, or modulo mutant.
        assert!((MiB::new(1024).unwrap().as_gib_f64() - 1.0).abs() < f64::EPSILON);
        assert!((MiB::new(2048).unwrap().as_gib_f64() - 2.0).abs() < f64::EPSILON);
        assert!((MiB::new(512).unwrap().as_gib_f64() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn gib_rejects_zero() {
        assert!(GiB::new(0).is_none());
        assert!(GiB::new(1).is_some());
    }

    #[test]
    fn zero_vcpu_rejected_at_parse() {
        let json = r#"{"vm": {"vcpu_count": 0}}"#;
        assert!(serde_json::from_str::<CoopConfig>(json).is_err());
    }

    #[test]
    fn zero_ssh_port_rejected_at_parse() {
        let json = r#"{"ssh_port": 0}"#;
        assert!(serde_json::from_str::<CoopConfig>(json).is_err());
    }

    #[test]
    fn zero_mem_rejected_at_parse() {
        let json = r#"{"vm": {"mem_size_mib": 0}}"#;
        assert!(serde_json::from_str::<CoopConfig>(json).is_err());
    }

    #[test]
    fn zero_template_size_rejected_at_parse() {
        let json = r#"{"vm": {"template_size_gib": 0}}"#;
        assert!(serde_json::from_str::<CoopConfig>(json).is_err());
    }

    // ── InstanceName ──────────────────────────────────────────

    #[test]
    fn instance_name_roundtrip_serde() {
        let name = InstanceName::new("my-vm").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, r#""my-vm""#);
        let loaded: InstanceName = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, name);
    }

    #[test]
    fn instance_name_rejects_invalid_on_deserialize() {
        let json = r#""../evil""#;
        assert!(serde_json::from_str::<InstanceName>(json).is_err());
    }

    #[test]
    fn instance_name_compares_to_str() {
        let name = InstanceName::new("foo").unwrap();
        assert!(name == *"foo");
        assert!(name != *"bar");
    }

    // ── ImageName ─────────────────────────────────────────────

    #[test]
    fn image_name_accepts_valid_identifiers() {
        for s in [
            "default",
            "python-dev",
            "alpha_beta",
            "img.1",
            "a",
            "A1",
            "ubuntu24.04",
            "x".repeat(64).as_str(),
        ] {
            ImageName::new(s).unwrap_or_else(|e| panic!("{s} should be valid: {e}"));
        }
    }

    #[test]
    fn image_name_rejects_empty() {
        assert!(ImageName::new("").is_err());
    }

    #[test]
    fn image_name_rejects_leading_dot() {
        // '.' and '..' would resolve to the images dir itself / its
        // parent (the directory-traversal motivation for this newtype).
        // Banning any leading '.' also keeps dotfile-style names out.
        for s in [".", "..", "..hidden", ".gitkeep", ".x"] {
            assert!(ImageName::new(s).is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn image_name_rejects_out_of_charset() {
        // Smoke test that the constructor wires through to
        // `validate_safe_chars`; the exhaustive char-class rejection is
        // covered by the `naming` module's tests.
        assert!(ImageName::new("with space").is_err());
    }

    #[test]
    fn image_name_rejects_overlong() {
        let too_long = "a".repeat(MAX_IMAGE_NAME_LEN + 1);
        assert!(ImageName::new(&too_long).is_err());
    }

    #[test]
    fn image_name_compares_to_str() {
        let name = ImageName::new("foo").unwrap();
        assert!(name == *"foo");
        assert!(name != *"bar");
    }

    #[test]
    fn image_name_roundtrip_serde() {
        let name = ImageName::new("python-dev").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, r#""python-dev""#);
        let loaded: ImageName = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, name);
    }

    #[test]
    fn image_name_rejects_invalid_on_deserialize() {
        // Smoke test that the `Deserialize` impl routes through
        // `ImageName::new`; per-rule rejection is covered by the
        // dedicated constructor tests.
        assert!(serde_json::from_str::<ImageName>(r#""../evil""#).is_err());
    }

    /// Pins the invariant relied on by `default_image_name`, which
    /// bypasses the validating constructor.
    #[test]
    fn default_image_is_valid() {
        ImageName::new(DEFAULT_IMAGE).unwrap();
        assert_eq!(default_image_name().as_str(), DEFAULT_IMAGE);
    }

    // ── SubnetMask ────────────────────────────────────────────

    #[test]
    fn subnet_mask_new_accepts_in_range() {
        for bits in [0_u8, 1, 24, 32] {
            assert!(SubnetMask::new(bits).is_some());
        }
    }

    #[test]
    fn subnet_mask_new_rejects_out_of_range() {
        assert!(SubnetMask::new(33).is_none());
        assert!(SubnetMask::new(255).is_none());
    }

    #[test]
    fn subnet_mask_fromstr_accepts_slash_and_bare() {
        assert_eq!(
            "/24".parse::<SubnetMask>().unwrap(),
            SubnetMask::new(24).unwrap()
        );
        assert_eq!(
            "24".parse::<SubnetMask>().unwrap(),
            SubnetMask::new(24).unwrap()
        );
        assert_eq!(
            "/0".parse::<SubnetMask>().unwrap(),
            SubnetMask::new(0).unwrap()
        );
        assert_eq!(
            "/32".parse::<SubnetMask>().unwrap(),
            SubnetMask::new(32).unwrap()
        );
    }

    #[test]
    fn subnet_mask_fromstr_rejects_invalid() {
        assert!("/33".parse::<SubnetMask>().is_err());
        assert!("33".parse::<SubnetMask>().is_err());
        assert!("abc".parse::<SubnetMask>().is_err());
        assert!("255.255.255.0".parse::<SubnetMask>().is_err());
        assert!("".parse::<SubnetMask>().is_err());
        assert!("/".parse::<SubnetMask>().is_err());
        assert!("/-1".parse::<SubnetMask>().is_err());
    }

    #[test]
    fn subnet_mask_roundtrip_serde_json() {
        let mask = SubnetMask::new(24).unwrap();
        let json = serde_json::to_string(&mask).unwrap();
        assert_eq!(json, r#""/24""#);
        let loaded: SubnetMask = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, mask);
    }

    #[test]
    fn subnet_mask_deserialize_accepts_string_and_integer() {
        assert_eq!(
            serde_json::from_str::<SubnetMask>(r#""/24""#).unwrap(),
            SubnetMask::new(24).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<SubnetMask>(r#""24""#).unwrap(),
            SubnetMask::new(24).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<SubnetMask>("24").unwrap(),
            SubnetMask::new(24).unwrap()
        );
    }

    #[test]
    fn subnet_mask_deserialize_rejects_out_of_range() {
        assert!(serde_json::from_str::<SubnetMask>(r#""/33""#).is_err());
        assert!(serde_json::from_str::<SubnetMask>("33").is_err());
        assert!(serde_json::from_str::<SubnetMask>("-1").is_err());
    }

    #[test]
    fn config_load_accepts_subnet_mask_string_and_integer() {
        let tmp = TempDir::new().unwrap();

        let with_slash = tmp.path().join("slash.toml");
        fs::write(&with_slash, "[network]\nsubnet_mask = \"/16\"\n").unwrap();
        let cfg = CoopConfig::load(&with_slash).unwrap();
        assert_eq!(cfg.network.subnet_mask, SubnetMask::new(16).unwrap());

        let bare = tmp.path().join("bare.toml");
        fs::write(&bare, "[network]\nsubnet_mask = \"16\"\n").unwrap();
        let cfg = CoopConfig::load(&bare).unwrap();
        assert_eq!(cfg.network.subnet_mask, SubnetMask::new(16).unwrap());

        let int = tmp.path().join("int.toml");
        fs::write(&int, "[network]\nsubnet_mask = 16\n").unwrap();
        let cfg = CoopConfig::load(&int).unwrap();
        assert_eq!(cfg.network.subnet_mask, SubnetMask::new(16).unwrap());
    }

    #[test]
    fn config_load_rejects_invalid_subnet_mask() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[network]\nsubnet_mask = \"/33\"\n").unwrap();
        assert!(CoopConfig::load(&path).is_err());
    }

    // ── InterfaceName / HostInterface ─────────────────────────

    #[test]
    fn interface_name_accepts_typical_linux_names() {
        for s in [
            "eth0", "ens5", "en0", "wlan0", "br0", "veth-1", "tap_0", "lo",
        ] {
            assert!(InterfaceName::new(s).is_ok(), "{s} should be accepted");
        }
    }

    #[test]
    fn interface_name_rejects_empty_and_reserved() {
        assert!(InterfaceName::new("").is_err());
        assert!(InterfaceName::new(".").is_err());
        assert!(InterfaceName::new("..").is_err());
    }

    #[test]
    fn interface_name_rejects_overlong() {
        let too_long = "a".repeat(MAX_INTERFACE_NAME_LEN + 1);
        assert!(InterfaceName::new(&too_long).is_err());
        let max_ok = "a".repeat(MAX_INTERFACE_NAME_LEN);
        assert!(InterfaceName::new(&max_ok).is_ok());
    }

    #[test]
    fn interface_name_rejects_out_of_charset() {
        // Smoke test that the constructor wires through to
        // `validate_safe_chars`; the exhaustive char-class rejection is
        // covered by the `naming` module's tests.
        assert!(InterfaceName::new("eth 0").is_err());
    }

    #[test]
    fn host_interface_deserializes_auto_sentinel() {
        let parsed: HostInterface = serde_json::from_str(r#""auto""#).unwrap();
        assert_eq!(parsed, HostInterface::Auto);
    }

    #[test]
    fn host_interface_deserializes_named_interface() {
        let parsed: HostInterface = serde_json::from_str(r#""eth0""#).unwrap();
        assert_eq!(
            parsed,
            HostInterface::Named(InterfaceName::new("eth0").unwrap())
        );
    }

    /// The whole point of the enum: typo'd sentinels fail loudly rather
    /// than silently bypassing auto-detection.
    #[test]
    fn host_interface_rejects_sentinel_typos() {
        for s in [r#""Auto""#, r#""AUTO""#, r#"" auto""#, r#""auto ""#] {
            assert!(
                serde_json::from_str::<HostInterface>(s).is_err(),
                "{s} should not be accepted as the auto sentinel"
            );
        }
    }

    #[test]
    fn host_interface_rejects_invalid_interface_name() {
        // Smoke test that `HostInterface`'s `Deserialize` wraps
        // `InterfaceName::new` for non-`auto` values; the per-rule
        // rejection is covered by the constructor tests.
        assert!(serde_json::from_str::<HostInterface>(r#""eth/0""#).is_err());
    }

    #[test]
    fn host_interface_roundtrip_serde() {
        let auto = HostInterface::Auto;
        assert_eq!(serde_json::to_string(&auto).unwrap(), r#""auto""#);

        let named = HostInterface::Named(InterfaceName::new("ens5").unwrap());
        assert_eq!(serde_json::to_string(&named).unwrap(), r#""ens5""#);
    }

    #[test]
    fn config_load_rejects_typo_host_iface() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        // " auto" with a leading space — the bug the enum exists to prevent.
        fs::write(&path, "[network]\nhost_iface = \" auto\"\n").unwrap();
        assert!(CoopConfig::load(&path).is_err());
    }

    #[test]
    fn load_valid_toml_parses() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "ssh_port = 2222\n\n[vm]\nvcpu_count = 4\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(cfg.ssh_port.get(), 2222);
        assert_eq!(cfg.vm.vcpu_count.get(), 4);
        // Unspecified fields get defaults
        assert_eq!(cfg.vm.mem_size_mib.get(), MiB::new(4096).unwrap());
        assert_eq!(cfg.network.host_ip, Ipv4Addr::new(172, 16, 0, 1));
    }

    #[test]
    fn load_invalid_toml_errors() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "not valid toml [[[").unwrap();

        let err = CoopConfig::load(&path).unwrap_err();
        assert!(
            err.to_string().contains("parse") || err.to_string().contains("TOML"),
            "expected parse error, got: {err}"
        );
    }

    #[test]
    fn load_empty_toml_uses_all_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        let defaults = CoopConfig::default();
        assert_eq!(cfg.vm.vcpu_count, defaults.vm.vcpu_count);
        assert_eq!(cfg.vm.mem_size_mib, defaults.vm.mem_size_mib);
        assert_eq!(cfg.ssh_port, defaults.ssh_port);
        assert_eq!(cfg.network.host_ip, defaults.network.host_ip);
        assert_eq!(cfg.network.subnet_mask, defaults.network.subnet_mask);
        assert_eq!(cfg.network.host_iface, defaults.network.host_iface);
        assert_eq!(cfg.vm.template_size_gib, defaults.vm.template_size_gib);
    }

    #[test]
    fn load_partial_vm_config_fills_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[vm]\nmem_size_mib = 8192\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(cfg.vm.mem_size_mib.get(), MiB::new(8192).unwrap());
        assert_eq!(cfg.vm.vcpu_count.get(), 2);
        assert_eq!(cfg.vm.template_size_gib, GiB::new(8).unwrap());
    }

    #[test]
    fn load_partial_network_config_fills_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[network]\nhost_iface = \"eth0\"\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(
            cfg.network.host_iface,
            HostInterface::Named(InterfaceName::new("eth0").unwrap())
        );
        assert_eq!(cfg.network.host_ip, Ipv4Addr::new(172, 16, 0, 1));
        assert_eq!(cfg.network.subnet_mask, SubnetMask::new(24).unwrap());
    }

    #[test]
    fn load_unknown_fields_ignored() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "ssh_port = 2222\nunknown_field = true\n").unwrap();

        let result = CoopConfig::load(&path);
        // serde default behavior: unknown fields rejected unless
        // deny_unknown_fields is NOT set. Verify current behavior.
        assert!(result.is_ok(), "unknown fields should be ignored");
    }

    #[test]
    fn load_json_backward_compat() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("coop.json");
        fs::write(&path, r#"{"ssh_port": 2222, "vm": {"vcpu_count": 4}}"#).unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(cfg.ssh_port.get(), 2222);
        assert_eq!(cfg.vm.vcpu_count.get(), 4);
    }

    // ── Path construction ─────────────────────────────────────

    /// Every path getter is a deterministic `join` under a known root.
    /// One test pins the full set — instance dir, config `data_dir`, and
    /// the default-value getters — since each is just string composition
    /// and a single mutation per getter would otherwise survive.
    #[test]
    fn paths_compose_from_their_roots() {
        // Instance paths join under the instance dir.
        let inst = test_inst("foo", idx(0), PathBuf::from("/data/instances/foo"));
        for (got, want) in [
            (inst.rootfs_path(), "/data/instances/foo/rootfs.ext4"),
            (inst.pid_file_path(), "/data/instances/foo/firecracker.pid"),
            (
                inst.api_socket_path(),
                "/data/instances/foo/firecracker.socket",
            ),
            (inst.log_path(), "/data/instances/foo/firecracker.log"),
            (inst.vsock_path(), "/data/instances/foo/vsock.sock"),
            (inst.vm_config_path(), "/data/instances/foo/vm_config.json"),
            (
                inst.forwards_state_path(),
                "/data/instances/foo/forwards.json",
            ),
        ] {
            assert_eq!(got, PathBuf::from(want));
        }

        // Config getters join under `data_dir`.
        let cfg = CoopConfig {
            data_dir: ConfigPath::new("/my/data"),
            ..CoopConfig::default()
        };
        let python_dev = ImageName::new("python-dev").unwrap();
        for (got, want) in [
            (
                cfg.template_path(),
                "/my/data/images/default/rootfs-template.ext4",
            ),
            (
                cfg.template_config_path_for(&default_img()),
                "/my/data/images/default/template-config.json",
            ),
            (
                cfg.template_path_for(&python_dev),
                "/my/data/images/python-dev/rootfs-template.ext4",
            ),
            (
                cfg.lima_base_path(&python_dev),
                "/my/data/images/python-dev/lima-base.img",
            ),
            (cfg.ssh_key_path(), "/my/data/vm_key"),
            (cfg.instances_dir(), "/my/data/instances"),
            (cfg.images_dir(), "/my/data/images"),
        ] {
            assert_eq!(got, PathBuf::from(want));
        }

        // Default-value getters compose the same filenames under the
        // default data dir.
        let data = default_data_dir();
        assert_eq!(default_kernel_path(), ConfigPath::new(data.join("vmlinux")));
        assert_eq!(
            default_firecracker_bin(),
            ConfigPath::new(data.join("firecracker"))
        );
    }

    #[test]
    fn default_data_dir_is_under_home() {
        let dir = default_data_dir();
        assert!(
            dir.ends_with(".coop"),
            "expected path ending with .coop, got: {dir:?}"
        );
    }

    // ── Config validation ─────────────────────────────────────

    #[test]
    fn validate_defaults_pass() {
        let cfg = CoopConfig::default();
        let warnings = cfg.validate().unwrap();
        // Warnings are acceptable; errors are not
        assert!(warnings.len() <= 3, "unexpected warnings: {warnings:?}");
    }

    // Zero-value rejection is enforced by NonZero types at deserialization
    // time (tested in the "NonZero type enforcement" section above). The
    // guest-memory floor is enforced by construction via `VmMemory::new`
    // (tested below), not in `validate()` — so an unbootable value is
    // unrepresentable rather than caught by a late pass.

    #[test]
    fn vm_memory_rejects_below_minimum() {
        let err = VmMemory::new(MiB::new(127).unwrap()).unwrap_err();
        assert!(err.to_string().contains("mem_size_mib=127 is too low"));
    }

    #[test]
    fn vm_memory_accepts_minimum_and_above() {
        assert_eq!(VmMemory::new(MIN_MEM_MIB).unwrap().get(), MIN_MEM_MIB);
        assert_eq!(
            VmMemory::new(MiB::new(4096).unwrap()).unwrap().get(),
            MiB::new(4096).unwrap()
        );
    }

    #[test]
    fn vm_memory_parse_cli_enforces_floor() {
        assert!(VmMemory::parse_cli("16").is_err());
        assert!(VmMemory::parse_cli("0").is_err());
        assert_eq!(VmMemory::parse_cli("128").unwrap().get(), MIN_MEM_MIB);
    }

    #[test]
    fn vm_memory_deserialize_enforces_floor() {
        // config.toml with a below-floor mem_size_mib is rejected at load.
        let err = toml::from_str::<CoopConfig>("[vm]\nmem_size_mib = 16\n").unwrap_err();
        assert!(
            err.to_string().contains("is too low"),
            "expected floor error, got: {err}"
        );
        let cfg: CoopConfig = toml::from_str("[vm]\nmem_size_mib = 256\n").unwrap();
        assert_eq!(cfg.vm.mem_size_mib.get(), MiB::new(256).unwrap());
    }

    #[test]
    fn deserialize_rejects_invalid_host_ip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[network]\nhost_ip = \"not-an-ip\"\n").unwrap();
        let err = CoopConfig::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid value") || msg.contains("parse") || msg.contains("TOML"),
            "expected IP parse error, got: {msg}"
        );
    }

    #[test]
    fn validate_rejects_missing_local_marketplace() {
        let mut cfg = CoopConfig::default();
        cfg.claude.marketplaces = vec!["/nonexistent/skills-dir".into()];
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("claude.marketplaces"),
            "expected marketplace error, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_url_marketplace() {
        let cfg = CoopConfig {
            claude: ClaudeConfig {
                marketplaces: vec!["https://github.com/anthropics/plugins".into()],
                ..ClaudeConfig::default()
            },
            ..CoopConfig::default()
        };
        // URL is not an absolute path, so no local-path validation
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_accepts_existing_local_marketplace() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("my-skills");
        fs::create_dir(&dir).unwrap();

        let mut cfg = CoopConfig::default();
        cfg.claude.marketplaces = vec![dir.to_string_lossy().into_owned()];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn load_expands_tilde_in_marketplaces() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[claude]\nmarketplaces = [\"~/my-skills\"]\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        let mp = &cfg.claude.marketplaces[0];
        assert!(!mp.starts_with('~'), "tilde should be expanded, got: {mp}");
        assert!(
            mp.contains("/my-skills"),
            "should preserve path suffix, got: {mp}"
        );
    }

    #[test]
    fn load_preserves_url_marketplaces() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let url = "https://github.com/anthropics/plugins";
        fs::write(&path, format!("[claude]\nmarketplaces = [\"{url}\"]\n")).unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(cfg.claude.marketplaces[0], url);
    }

    #[test]
    fn validate_rejects_missing_local_codex_marketplace() {
        let mut cfg = CoopConfig::default();
        cfg.codex.marketplaces = vec!["/nonexistent/codex-plugins".into()];
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("codex.marketplaces"),
            "expected codex marketplace error, got: {err}"
        );
    }

    #[test]
    fn load_expands_tilde_in_codex_marketplaces() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[codex]\nmarketplaces = [\"~/codex-plugins\"]\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        let mp = &cfg.codex.marketplaces[0];
        assert!(!mp.starts_with('~'), "tilde should be expanded, got: {mp}");
        assert!(
            mp.contains("/codex-plugins"),
            "should preserve path suffix, got: {mp}"
        );
    }

    #[test]
    fn validate_collects_multiple_errors() {
        let mut cfg = CoopConfig::default();
        cfg.claude.config_dir = ConfigDir::Custom(ConfigPath::new("/nonexistent/claude-config"));
        cfg.codex.config_dir = ConfigDir::Custom(ConfigPath::new("/nonexistent/codex-config"));
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("claude.config_dir"),
            "missing claude config_dir error: {msg}"
        );
        assert!(
            msg.contains("codex.config_dir"),
            "missing codex config_dir error: {msg}"
        );
    }

    // ── validate() boundary pinning (issue #137) ──────────────
    // Each test here exists to kill a specific surviving mutant
    // from `cargo mutants -f src/config.rs`. Do not loosen the
    // assertions without re-running mutants first.

    /// Build a `CoopConfig` with all path fields rooted in `data_dir`
    /// so existence checks are deterministic. By default both
    /// `kernel_path` and `firecracker_bin` point at non-existent files.
    fn validate_fixture(data_dir: &Path) -> CoopConfig {
        CoopConfig {
            data_dir: ConfigPath::new(data_dir),
            vm: VmConfig {
                kernel_path: ConfigPath::new(data_dir.join("nonexistent-kernel")),
                ..VmConfig::default()
            },
            firecracker_bin: ConfigPath::new(data_dir.join("nonexistent-firecracker")),
            ..CoopConfig::default()
        }
    }

    // The `mem_size_mib < 128` vs `<= 128` boundary is pinned by
    // `vm_memory_rejects_below_minimum` / `vm_memory_accepts_minimum_and_above`
    // now that the floor lives in `VmMemory::new`, not `validate()`.

    // Pins both `!parent.as_os_str().is_empty()` and `!parent.exists()`:
    // with a non-empty, non-existent parent, the warning must fire.
    #[test]
    fn validate_warns_when_data_dir_parent_missing() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("absent").join("coop");
        let cfg = validate_fixture(&data_dir);
        let warnings = cfg.validate().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("data_dir parent") && w.contains("does not exist")),
            "expected data_dir parent warning, got {warnings:?}"
        );
    }

    // Pins `!parent.exists()`: with an existing parent, the warning
    // must NOT fire (catches the negation being deleted).
    #[test]
    fn validate_no_data_dir_warning_when_parent_exists() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("coop");
        let cfg = validate_fixture(&data_dir);
        let warnings = cfg.validate().unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("data_dir parent")),
            "unexpected data_dir warning, got {warnings:?}"
        );
    }

    // Pins `!self.vm.kernel_path.exists()` (forward direction): when
    // the template is present and the kernel is absent, warn.
    #[test]
    fn validate_warns_kernel_missing_when_template_exists() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        let image_dir = data_dir.join("images").join(DEFAULT_IMAGE);
        fs::create_dir_all(&image_dir).unwrap();
        fs::write(image_dir.join("rootfs-template.ext4"), b"").unwrap();

        let cfg = validate_fixture(&data_dir);
        let warnings = cfg.validate().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("kernel_path") && w.contains("does not exist")),
            "expected kernel_path warning, got {warnings:?}"
        );
    }

    // Pins `!self.vm.kernel_path.exists()` (reverse direction): when
    // the kernel is present, no warning even with the template built.
    #[test]
    fn validate_no_kernel_warning_when_kernel_exists() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        let image_dir = data_dir.join("images").join(DEFAULT_IMAGE);
        fs::create_dir_all(&image_dir).unwrap();
        fs::write(image_dir.join("rootfs-template.ext4"), b"").unwrap();
        let kernel = data_dir.join("vmlinux");
        fs::write(&kernel, b"").unwrap();

        let mut cfg = validate_fixture(&data_dir);
        cfg.vm.kernel_path = ConfigPath::new(kernel);
        let warnings = cfg.validate().unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("kernel_path")),
            "unexpected kernel_path warning, got {warnings:?}"
        );
    }

    // Pins the outer `template_path().exists()` gate: when the
    // template is absent, the kernel check is skipped entirely.
    #[test]
    fn validate_no_kernel_warning_when_template_missing() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        let cfg = validate_fixture(&data_dir);
        let warnings = cfg.validate().unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("kernel_path")),
            "kernel check should be gated on template existence, got {warnings:?}"
        );
    }

    // Pins `!self.firecracker_bin.exists()` (forward) on Linux.
    #[test]
    #[cfg(target_os = "linux")]
    fn validate_warns_firecracker_missing_on_linux() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        let cfg = validate_fixture(&data_dir);
        let warnings = cfg.validate().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("firecracker_bin") && w.contains("does not exist")),
            "expected firecracker_bin warning on Linux, got {warnings:?}"
        );
    }

    // Pins `&&` between firecracker existence and the linux cfg: on
    // Linux when firecracker IS present, no warning must fire.
    // Mutation to `||` would always warn on Linux.
    #[test]
    #[cfg(target_os = "linux")]
    fn validate_no_firecracker_warning_when_present_on_linux() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        let firecracker = data_dir.join("firecracker");
        fs::write(&firecracker, b"").unwrap();

        let mut cfg = validate_fixture(&data_dir);
        cfg.firecracker_bin = ConfigPath::new(firecracker);
        let warnings = cfg.validate().unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("firecracker_bin")),
            "unexpected firecracker_bin warning, got {warnings:?}"
        );
    }

    // Pins the linux gate on non-linux hosts: a missing firecracker
    // binary must not warn off-Linux.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn validate_no_firecracker_warning_on_non_linux() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        let cfg = validate_fixture(&data_dir);
        let warnings = cfg.validate().unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("firecracker_bin")),
            "firecracker_bin warning must be gated on Linux, got {warnings:?}"
        );
    }

    // Pins `!parent.as_os_str().is_empty()`: a bare relative `data_dir`
    // has an empty parent (`Path::new("coop").parent() == Some("")`).
    // Deleting the emptiness guard would make the warning fire because
    // an empty path "does not exist", so this must stay silent.
    #[test]
    fn validate_no_data_dir_warning_when_parent_empty() {
        // A single-component relative path: `Path::parent` yields `Some("")`.
        let cfg = validate_fixture(Path::new("coop"));
        let warnings = cfg.validate().unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("data_dir parent")),
            "bare relative data_dir has an empty parent; no warning expected, got {warnings:?}"
        );
    }

    // Pins the `parent.exists()` arm at the filesystem root: `/` always
    // exists, so a `data_dir` of `/coop` must not warn. Catches a mutant
    // that forces the existence check to a constant.
    #[test]
    fn validate_no_data_dir_warning_when_parent_is_root() {
        let cfg = validate_fixture(Path::new("/coop-data"));
        let warnings = cfg.validate().unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("data_dir parent")),
            "root parent '/' exists; no warning expected, got {warnings:?}"
        );
    }

    // Pins the marketplace loop accumulating one error per offending
    // entry: with two non-existent absolute paths, both must surface.
    // A mutant that `break`s after the first, or skips the push, drops
    // one of the two names.
    #[test]
    fn validate_collects_all_marketplace_errors() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = validate_fixture(tmp.path());
        cfg.claude.marketplaces = vec![
            "/nonexistent/marketplace-a".to_string(),
            "/nonexistent/marketplace-b".to_string(),
        ];
        let msg = cfg.validate().unwrap_err().to_string();
        assert!(
            msg.contains("marketplace-a"),
            "missing first marketplace error: {msg}"
        );
        assert!(
            msg.contains("marketplace-b"),
            "missing second marketplace error: {msg}"
        );
    }

    // ── Custom profiles ──────────────────────────────────────

    #[test]
    fn custom_profiles_deserialize() {
        let json = r#"{
            "profiles": {
                "data-science": {
                    "apt_packages": ["python3", "libopenblas-dev"],
                    "post_install": "pip3 install numpy"
                }
            }
        }"#;
        let cfg: CoopConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.profiles.len(), 1);
        let ds = &cfg.profiles["data-science"];
        assert_eq!(ds.apt_packages, vec!["python3", "libopenblas-dev"]);
        assert_eq!(ds.post_install.as_deref(), Some("pip3 install numpy"));
        assert!(ds.pre_install.is_none());
    }

    #[test]
    fn custom_profiles_default_empty() {
        let cfg = CoopConfig::default();
        assert!(cfg.profiles.is_empty());
    }

    // ── post_start ───────────────────────────────────────────

    #[test]
    fn post_start_deserializes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "post_start = \"touch /tmp/booted\"\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(cfg.post_start.as_deref(), Some("touch /tmp/booted"));
    }

    #[test]
    fn post_start_default_none() {
        let cfg = CoopConfig::default();
        assert!(cfg.post_start.is_none());
    }

    // ── Named images ─────────────────────────────────────────

    #[test]
    fn image_dir_paths() {
        let cfg = CoopConfig {
            data_dir: ConfigPath::new("/data"),
            ..CoopConfig::default()
        };
        let foo = ImageName::new("foo").unwrap();
        assert_eq!(cfg.image_dir(&foo), PathBuf::from("/data/images/foo"));
        assert_eq!(
            cfg.template_path_for(&foo),
            PathBuf::from("/data/images/foo/rootfs-template.ext4")
        );
        assert_eq!(
            cfg.template_config_path_for(&foo),
            PathBuf::from("/data/images/foo/template-config.json")
        );
        assert_eq!(
            cfg.lima_base_path(&foo),
            PathBuf::from("/data/images/foo/lima-base.img")
        );
        assert_eq!(
            cfg.lima_template_path(&foo),
            PathBuf::from("/data/images/foo/lima-template.yaml")
        );
    }

    #[test]
    fn list_images_empty() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        let images = cfg.list_images().unwrap();
        assert!(images.is_empty());
    }

    #[test]
    fn list_images_finds_dirs() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        fs::create_dir_all(cfg.image_dir(&ImageName::new("alpha").unwrap())).unwrap();
        fs::create_dir_all(cfg.image_dir(&ImageName::new("beta").unwrap())).unwrap();
        let images = cfg.list_images().unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].name.as_str(), "alpha");
        assert_eq!(images[1].name.as_str(), "beta");
    }

    /// Image dirs whose names can't be parsed as [`ImageName`] (e.g. a
    /// stray dotfile or an entry hand-edited in) are silently skipped
    /// rather than poisoning the listing — same behaviour as
    /// `list_instances` for corrupted instance dirs.
    #[test]
    fn list_images_skips_invalid_names() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        fs::create_dir_all(cfg.images_dir().join("..hidden")).unwrap();
        fs::create_dir_all(cfg.images_dir().join("with space")).unwrap();
        fs::create_dir_all(cfg.image_dir(&ImageName::new("ok").unwrap())).unwrap();
        let images = cfg.list_images().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].name.as_str(), "ok");
    }

    #[test]
    fn instance_save_load_with_image() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("inst");
        let inst = Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: dir.clone(),
            image: ImageName::new("python-dev").unwrap(),
        };
        inst.save().unwrap();
        let loaded = Instance::load(&dir).unwrap();
        assert_eq!(loaded.image.as_str(), "python-dev");
    }

    #[test]
    fn instance_load_missing_image_defaults() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("inst");
        fs::create_dir_all(&dir).unwrap();
        // Write old-format instance.json without image field
        fs::write(dir.join("instance.json"), r#"{"name": "test", "index": 0}"#).unwrap();
        let loaded = Instance::load(&dir).unwrap();
        assert_eq!(loaded.image.as_str(), DEFAULT_IMAGE);
    }

    /// Pre-`ImageName` instances stored arbitrary strings here; a name
    /// that fails validation should be rejected at load time rather than
    /// silently used to construct paths.
    #[test]
    fn instance_load_rejects_invalid_image_name() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("inst");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("instance.json"),
            r#"{"name": "test", "index": 0, "image": "../escape"}"#,
        )
        .unwrap();
        let err = Instance::load(&dir).unwrap_err();
        let chain = err.chain().fold(String::new(), |mut acc, e| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{e} | ");
            acc
        });
        assert!(
            chain.contains("must not start with") || chain.contains("invalid character"),
            "expected validation error, got: {chain}"
        );
    }

    // ── Resilience: corrupted instance dirs ─────────────────

    #[test]
    fn list_skips_dir_without_instance_json() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "good", idx(0));

        // Create a dir with no instance.json (crashed mid-create)
        let orphan = tmp.path().join("instances").join("orphan");
        fs::create_dir_all(&orphan).unwrap();

        let instances = cfg.list_instances().unwrap();
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].name, *"good");
    }

    #[test]
    fn list_skips_dir_with_corrupted_json() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "good", idx(0));

        // Create a dir with garbage JSON (truncated write)
        let broken = tmp.path().join("instances").join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("instance.json"), r#"{"name": "bro"#).unwrap();

        let instances = cfg.list_instances().unwrap();
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].name, *"good");
    }

    #[test]
    fn list_skips_dir_with_empty_instance_json() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "good", idx(0));

        // Create a dir with empty file (truncated before any content)
        let empty = tmp.path().join("instances").join("empty");
        fs::create_dir_all(&empty).unwrap();
        fs::write(empty.join("instance.json"), "").unwrap();

        let instances = cfg.list_instances().unwrap();
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].name, *"good");
    }

    #[test]
    fn allocate_works_alongside_corrupted_dirs() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        // Create a corrupted instance dir occupying no valid index
        let broken = tmp.path().join("instances").join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("instance.json"), "not json").unwrap();

        // Allocation should succeed — corrupted dirs are skipped
        let inst = cfg
            .allocate_instance(None, &ImageName::new(DEFAULT_IMAGE).unwrap(), None)
            .unwrap();
        assert_eq!(inst.index.as_u16(), 0);
    }

    #[test]
    fn instance_save_overwrites_atomically() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("inst");

        // Save initial state
        let inst = Instance {
            name: InstanceName::new("v1").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: dir.clone(),
            image: ImageName::new(DEFAULT_IMAGE).unwrap(),
        };
        inst.save().unwrap();

        // Overwrite with different content
        let inst2 = Instance {
            name: InstanceName::new("v2").unwrap(),
            index: InstanceIndex::new(5).unwrap(),
            dir: dir.clone(),
            image: ImageName::new("custom").unwrap(),
        };
        inst2.save().unwrap();

        // Load should see the new content, not a mix
        let loaded = Instance::load(&dir).unwrap();
        assert_eq!(loaded.name, *"v2");
        assert_eq!(loaded.index.as_u16(), 5);
        assert_eq!(loaded.image.as_str(), "custom");

        // No temp file left behind
        assert!(!dir.join("instance.tmp").exists());
    }

    // ── DiskSize parsing ────────────────────────────────────

    #[test]
    fn disk_size_absolute_bare() {
        let ds = DiskSize::parse("150").unwrap();
        assert_eq!(ds, DiskSize::Absolute(GiB::new(150).unwrap()));
    }

    #[test]
    fn disk_size_absolute_with_suffix() {
        let ds = DiskSize::parse("150G").unwrap();
        assert_eq!(ds, DiskSize::Absolute(GiB::new(150).unwrap()));
    }

    #[test]
    fn disk_size_absolute_lowercase_suffix() {
        let ds = DiskSize::parse("150g").unwrap();
        assert_eq!(ds, DiskSize::Absolute(GiB::new(150).unwrap()));
    }

    #[test]
    fn disk_size_relative_bare() {
        let ds = DiskSize::parse("+20").unwrap();
        assert_eq!(ds, DiskSize::Relative(GiB::new(20).unwrap()));
    }

    #[test]
    fn disk_size_relative_with_suffix() {
        let ds = DiskSize::parse("+20G").unwrap();
        assert_eq!(ds, DiskSize::Relative(GiB::new(20).unwrap()));
    }

    #[test]
    fn disk_size_rejects_zero() {
        assert!(DiskSize::parse("0").is_err());
        assert!(DiskSize::parse("+0").is_err());
    }

    #[test]
    fn disk_size_rejects_non_numeric() {
        assert!(DiskSize::parse("abc").is_err());
        assert!(DiskSize::parse("+G").is_err());
    }

    #[test]
    fn disk_size_resolve_absolute() {
        let ds = DiskSize::Absolute(GiB::new(150).unwrap());
        let resolved = ds.resolve(GiB::new(100).unwrap()).unwrap();
        assert_eq!(resolved, GiB::new(150).unwrap());
    }

    #[test]
    fn disk_size_resolve_relative() {
        let ds = DiskSize::Relative(GiB::new(20).unwrap());
        let resolved = ds.resolve(GiB::new(100).unwrap()).unwrap();
        assert_eq!(resolved, GiB::new(120).unwrap());
    }

    #[test]
    fn disk_size_resolve_relative_overflows() {
        let ds = DiskSize::Relative(GiB::new(u32::MAX).unwrap());
        let err = ds.resolve(GiB::new(1).unwrap()).unwrap_err();
        assert!(err.to_string().contains("overflow"), "{err}");
    }

    // ── Mount parsing ───────────────────────────────────────────

    #[test]
    fn mount_parse_host_only_defaults_to_workspace() {
        let tmp = TempDir::new().unwrap();
        let m = Mount::parse(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(m.host_path, tmp.path().canonicalize().unwrap());
        assert_eq!(m.guest_path.to_string(), "/workspace");
    }

    #[test]
    fn mount_parse_host_and_guest() {
        let tmp = TempDir::new().unwrap();
        let spec = format!("{}:/data/project", tmp.path().display());
        let m = Mount::parse(&spec).unwrap();
        assert_eq!(m.host_path, tmp.path().canonicalize().unwrap());
        assert_eq!(m.guest_path.to_string(), "/data/project");
    }

    #[test]
    fn mount_parse_rejects_nonexistent_host() {
        let err = Mount::parse("/no/such/path/xyz").unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "expected 'does not exist', got: {err}"
        );
    }

    #[test]
    fn mount_parse_rejects_file_host() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("afile");
        fs::write(&file, "x").unwrap();
        let err = Mount::parse(file.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("not a directory"),
            "expected 'not a directory', got: {err}"
        );
    }

    #[test]
    fn mount_parse_rejects_relative_guest_path() {
        let tmp = TempDir::new().unwrap();
        let spec = format!("{}:relative/path", tmp.path().display());
        let err = Mount::parse(&spec).unwrap_err();
        assert!(
            err.to_string().contains("must be absolute"),
            "expected 'must be absolute', got: {err}"
        );
    }

    #[test]
    fn mount_host_is_git_repo_detects_git_directory() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        let m = Mount::parse(tmp.path().to_str().unwrap()).unwrap();
        assert!(m.host_is_git_repo());
    }

    #[test]
    fn mount_host_is_git_repo_detects_worktree_git_file() {
        // Linked worktrees have `.git` as a file pointing at the main
        // repo's gitdir, not a directory. Both should count.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        let m = Mount::parse(tmp.path().to_str().unwrap()).unwrap();
        assert!(m.host_is_git_repo());
    }

    #[test]
    fn mount_host_is_git_repo_false_for_plain_directory() {
        let tmp = TempDir::new().unwrap();
        let m = Mount::parse(tmp.path().to_str().unwrap()).unwrap();
        assert!(!m.host_is_git_repo());
    }

    // ── ConfigDir deserialization ────────────────────────────

    #[test]
    fn config_dir_deserializes_custom_path() {
        let json = r#"{"claude": {"config_dir": "/custom/path"}}"#;
        let cfg: CoopConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.claude.config_dir,
            ConfigDir::Custom(ConfigPath::new("/custom/path"))
        );
    }

    #[test]
    fn codex_config_dir_deserializes_custom_path() {
        let json = r#"{"codex": {"config_dir": "/custom/path"}}"#;
        let cfg: CoopConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.codex.config_dir,
            ConfigDir::Custom(ConfigPath::new("/custom/path"))
        );
    }

    #[test]
    fn config_dir_deserializes_disabled() {
        let json = r#"{"claude": {"config_dir": false}}"#;
        let cfg: CoopConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.claude.config_dir, ConfigDir::Disabled);
    }

    #[test]
    fn config_dir_deserializes_default_when_absent() {
        let json = r#"{"claude": {}}"#;
        let cfg: CoopConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.claude.config_dir, ConfigDir::Default);
    }

    #[test]
    fn config_dir_rejects_true() {
        let json = r#"{"claude": {"config_dir": true}}"#;
        let err = serde_json::from_str::<CoopConfig>(json).unwrap_err();
        assert!(
            err.to_string().contains("does not accept true"),
            "expected rejection of true, got: {err}"
        );
    }

    #[test]
    fn config_dir_rejects_number() {
        let json = r#"{"claude": {"config_dir": 42}}"#;
        assert!(serde_json::from_str::<CoopConfig>(json).is_err());
    }

    // ── ConfigDir validation ────────────────────────────────

    #[test]
    fn validate_passes_with_default_config_dir() {
        let cfg = CoopConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_with_existing_config_dir() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = CoopConfig::default();
        cfg.claude.config_dir = ConfigDir::Custom(ConfigPath::new(tmp.path()));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_with_existing_codex_config_dir() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = CoopConfig::default();
        cfg.codex.config_dir = ConfigDir::Custom(ConfigPath::new(tmp.path()));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_nonexistent_config_dir() {
        let mut cfg = CoopConfig::default();
        cfg.claude.config_dir = ConfigDir::Custom(ConfigPath::new("/nonexistent/config"));
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("config_dir"),
            "expected config_dir error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_nonexistent_codex_config_dir() {
        let mut cfg = CoopConfig::default();
        cfg.codex.config_dir = ConfigDir::Custom(ConfigPath::new("/nonexistent/config"));
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("codex.config_dir"),
            "expected codex config_dir error, got: {err}"
        );
    }

    #[test]
    fn validate_passes_with_disabled_config_dir() {
        let mut cfg = CoopConfig::default();
        cfg.claude.config_dir = ConfigDir::Disabled;
        assert!(cfg.validate().is_ok());
    }

    // ── ConfigDir tilde expansion ───────────────────────────

    #[test]
    fn load_expands_tilde_in_config_dir() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[claude]\nconfig_dir = \"~/foo\"\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        let ConfigDir::Custom(ref p) = cfg.claude.config_dir else {
            unreachable!("expected Custom, got: {:?}", cfg.claude.config_dir);
        };
        assert!(
            !p.starts_with("~"),
            "tilde should be expanded, got: {}",
            p.display()
        );
        assert!(
            p.to_string_lossy().contains("/foo"),
            "should preserve path suffix, got: {}",
            p.display()
        );
    }

    #[test]
    fn load_expands_tilde_in_codex_config_dir() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[codex]\nconfig_dir = \"~/foo\"\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        let ConfigDir::Custom(ref p) = cfg.codex.config_dir else {
            unreachable!("expected Custom, got: {:?}", cfg.codex.config_dir);
        };
        assert!(
            !p.starts_with("~"),
            "tilde should be expanded, got: {}",
            p.display()
        );
        assert!(
            p.to_string_lossy().contains("/foo"),
            "should preserve path suffix, got: {}",
            p.display()
        );
    }

    #[test]
    fn load_expands_tilde_in_data_dir() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "data_dir = \"~/coop-data\"\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert!(
            !cfg.data_dir.starts_with("~"),
            "tilde should be expanded, got: {}",
            cfg.data_dir.display()
        );
        assert_eq!(
            cfg.data_dir,
            ConfigPath::new(dirs::home_dir().unwrap().join("coop-data")),
            "should resolve to home directory"
        );
    }

    #[test]
    fn load_expands_tilde_in_firecracker_bin() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "firecracker_bin = \"~/bin/firecracker\"\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(
            cfg.firecracker_bin,
            ConfigPath::new(dirs::home_dir().unwrap().join("bin/firecracker")),
            "should resolve to home directory"
        );
    }

    #[test]
    fn load_expands_tilde_in_kernel_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[vm]\nkernel_path = \"~/kernels/vmlinux\"\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(
            cfg.vm.kernel_path,
            ConfigPath::new(dirs::home_dir().unwrap().join("kernels/vmlinux")),
            "should resolve to home directory"
        );
    }

    // The `ConfigPath` newtype expands `~` during deserialization, so the
    // invariant holds on every path into a `CoopConfig` — not only the
    // `load` wrapper. Pins the bypass that previously left a bare
    // `toml::from_str` / `serde_json::from_str` (and the `config_load`
    // fuzz target) with literal tildes.
    #[test]
    fn deserialize_expands_tilde_without_load() {
        let home = dirs::home_dir().unwrap();
        let toml_src = "data_dir = \"~/d\"\n\
             firecracker_bin = \"~/fc\"\n\
             [vm]\n\
             kernel_path = \"~/k\"\n\
             [claude]\n\
             config_dir = \"~/cc\"\n";

        let cfg: CoopConfig = toml::from_str(toml_src).unwrap();

        assert_eq!(cfg.data_dir, ConfigPath::new(home.join("d")));
        assert_eq!(cfg.firecracker_bin, ConfigPath::new(home.join("fc")));
        assert_eq!(cfg.vm.kernel_path, ConfigPath::new(home.join("k")));
        assert_eq!(
            cfg.claude.config_dir,
            ConfigDir::Custom(ConfigPath::new(home.join("cc")))
        );
    }

    #[test]
    fn load_expands_tilde_in_profile_marketplaces() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            "[profiles.custom]\nmarketplaces = [\"~/my-skills\", \"owner/repo\"]\n",
        )
        .unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        let mps = &cfg.profiles["custom"].marketplaces;
        assert!(
            !mps[0].starts_with('~'),
            "tilde should be expanded, got: {}",
            mps[0]
        );
        assert!(
            mps[0].contains("/my-skills"),
            "should preserve path suffix, got: {}",
            mps[0]
        );
        assert_eq!(
            mps[1], "owner/repo",
            "non-path marketplace entries should be left untouched"
        );
    }

    // ── Workspace-derived instance names ─────────────────────

    #[test]
    fn sanitize_basename_simple() {
        assert_eq!(sanitize_basename("myproject"), "myproject");
    }

    #[test]
    fn sanitize_basename_replaces_dots_and_spaces() {
        assert_eq!(sanitize_basename("my.project"), "my-project");
        assert_eq!(sanitize_basename("my project"), "my-project");
    }

    #[test]
    fn sanitize_basename_empty_returns_workspace() {
        assert_eq!(sanitize_basename(""), "workspace");
    }

    #[test]
    fn sanitize_basename_all_invalid_returns_dashes() {
        assert_eq!(sanitize_basename("..."), "---");
    }

    #[test]
    fn sanitize_basename_truncates_long_names() {
        let long = "a".repeat(100);
        let result = sanitize_basename(&long);
        assert_eq!(result.len(), 60);
    }

    #[test]
    fn sanitize_basename_preserves_hyphens_and_underscores() {
        assert_eq!(sanitize_basename("my-project_v2"), "my-project_v2");
    }

    #[test]
    fn unique_name_no_collision() {
        let instances = vec![];
        let name = unique_instance_name("foo", &instances).unwrap();
        assert_eq!(name, *"foo");
    }

    #[test]
    fn unique_name_with_collision() {
        let tmp = TempDir::new().unwrap();
        let inst = make_instance(tmp.path(), "foo", idx(0));
        let name = unique_instance_name("foo", &[inst]).unwrap();
        assert_eq!(name, *"foo-2");
    }

    #[test]
    fn unique_name_multiple_collisions() {
        let tmp = TempDir::new().unwrap();
        let i1 = make_instance(tmp.path(), "foo", idx(0));
        let i2 = make_instance(tmp.path(), "foo-2", idx(1));
        let name = unique_instance_name("foo", &[i1, i2]).unwrap();
        assert_eq!(name, *"foo-3");
    }

    #[test]
    fn allocate_with_workspace_derives_basename() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let ws = tmp.path().join("my-app");
        fs::create_dir(&ws).unwrap();

        let inst = cfg
            .allocate_instance(None, &default_img(), Some(&ws))
            .unwrap();
        assert_eq!(inst.name, *"my-app");
    }

    #[test]
    fn allocate_with_workspace_collision_appends_suffix() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let ws = tmp.path().join("dupe");
        fs::create_dir(&ws).unwrap();

        // First allocation takes the basename
        let inst1 = cfg
            .allocate_instance(None, &default_img(), Some(&ws))
            .unwrap();
        assert_eq!(inst1.name, *"dupe");

        // Second allocation with same basename gets -2 suffix
        let inst2 = cfg
            .allocate_instance(None, &default_img(), Some(&ws))
            .unwrap();
        assert_eq!(inst2.name, *"dupe-2");
    }

    #[test]
    fn allocate_explicit_name_overrides_workspace() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let ws = tmp.path().join("my-app");
        fs::create_dir(&ws).unwrap();

        let inst = cfg
            .allocate_instance(Some(&iname("custom")), &default_img(), Some(&ws))
            .unwrap();
        assert_eq!(inst.name, *"custom");
    }

    #[test]
    fn allocate_without_name_or_workspace_uses_index() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        let inst = cfg.allocate_instance(None, &default_img(), None).unwrap();
        assert_eq!(inst.name, *"0");
    }

    // ── cmd: prefix resolution ───────────────────────────────

    #[test]
    fn resolve_cmd_value_passthrough_plain() {
        let plain = "sk-ant-plain-value";
        assert_eq!(resolve_cmd_value(plain).unwrap(), plain);
    }

    #[test]
    fn resolve_cmd_value_passthrough_empty_string() {
        // Empty value (not cmd:) passes through; emptiness check
        // only applies to resolved cmd: output.
        assert_eq!(resolve_cmd_value("").unwrap(), "");
    }

    #[test]
    fn resolve_cmd_value_executes_command() {
        let resolved = resolve_cmd_value("cmd:echo hello").unwrap();
        assert_eq!(resolved, "hello");
    }

    #[test]
    fn resolve_cmd_value_trims_whitespace() {
        let resolved = resolve_cmd_value("cmd:printf '  padded  \\n\\n'").unwrap();
        assert_eq!(resolved, "padded");
    }

    #[test]
    fn resolve_cmd_value_trims_leading_space_in_prefix() {
        // `cmd: echo foo` (with space after colon) should still work
        let resolved = resolve_cmd_value("cmd: echo foo").unwrap();
        assert_eq!(resolved, "foo");
    }

    #[test]
    fn resolve_cmd_value_preserves_internal_whitespace() {
        let resolved = resolve_cmd_value("cmd:echo 'sk-ant token with spaces'").unwrap();
        assert_eq!(resolved, "sk-ant token with spaces");
    }

    #[test]
    fn resolve_cmd_value_supports_pipes() {
        // Verifies we run via `sh -c`, not direct exec
        let resolved = resolve_cmd_value("cmd:echo 'abc' | tr a-z A-Z").unwrap();
        assert_eq!(resolved, "ABC");
    }

    #[test]
    fn resolve_cmd_value_fails_on_nonzero_exit() {
        let err = resolve_cmd_value("cmd:false").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Secret command failed"),
            "expected failure message, got: {msg}"
        );
    }

    #[test]
    fn resolve_cmd_value_includes_stderr_on_failure() {
        let err = resolve_cmd_value("cmd:echo oops 1>&2; exit 1").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("oops"), "expected stderr in error, got: {msg}");
    }

    #[test]
    fn resolve_cmd_value_fails_on_empty_output() {
        let err = resolve_cmd_value("cmd:true").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty output"),
            "expected empty output error, got: {msg}"
        );
    }

    #[test]
    fn resolve_cmd_value_fails_on_empty_command() {
        let err = resolve_cmd_value("cmd:").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Empty command"),
            "expected empty command error, got: {msg}"
        );
    }

    #[test]
    fn resolve_cmd_value_fails_on_whitespace_only_command() {
        let err = resolve_cmd_value("cmd:   ").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Empty command"),
            "expected empty command error, got: {msg}"
        );
    }

    #[test]
    fn resolve_cmd_value_does_not_match_cmd_without_colon() {
        // `cmd` (no colon) is a plain value, not a command substitution
        assert_eq!(resolve_cmd_value("cmd").unwrap(), "cmd");
        assert_eq!(resolve_cmd_value("cmdline").unwrap(), "cmdline");
    }

    // ── Secret redaction ─────────────────────────────────────

    #[test]
    fn secret_debug_does_not_leak_value() {
        let s = Secret::new("real-token-value-do-not-leak".to_string());
        let debug = format!("{s:?}");
        assert!(
            !debug.contains("real-token-value-do-not-leak"),
            "Debug leaked secret value: {debug}"
        );
        assert!(
            debug.contains("redacted"),
            "Debug should mark redaction: {debug}"
        );
    }

    #[test]
    fn secret_expose_returns_underlying_value() {
        let s = Secret::new("plaintext".to_string());
        assert_eq!(s.expose(), "plaintext");
    }

    #[test]
    fn secret_serde_round_trip_is_transparent() {
        let json = r#""token-xyz""#;
        let s: Secret<String> = serde_json::from_str(json).unwrap();
        assert_eq!(s.expose(), "token-xyz");
        let out = serde_json::to_string(&s).unwrap();
        assert_eq!(out, json);
    }

    #[test]
    fn claude_config_api_key_debug_redacts() {
        let json = r#"{"api_key": "sk-ant-real-secret"}"#;
        let cfg: ClaudeConfig = serde_json::from_str(json).unwrap();
        let debug = format!("{cfg:?}");
        assert!(
            !debug.contains("sk-ant-real-secret"),
            "ClaudeConfig Debug leaked api_key: {debug}"
        );
    }

    #[test]
    fn codex_config_api_key_debug_redacts() {
        let json = r#"{"api_key": "sk-openai-real-secret"}"#;
        let cfg: CodexConfig = serde_json::from_str(json).unwrap();
        let debug = format!("{cfg:?}");
        assert!(
            !debug.contains("sk-openai-real-secret"),
            "CodexConfig Debug leaked api_key: {debug}"
        );
    }

    #[test]
    fn pat_entry_token_debug_redacts() {
        let entry = PatEntry {
            token: Secret::new("github_pat_secret".to_string()),
        };
        let debug = format!("{entry:?}");
        assert!(
            !debug.contains("github_pat_secret"),
            "PatEntry Debug leaked token: {debug}"
        );
    }

    #[test]
    fn mcp_resolved_header_debug_redacts() {
        let mut def = McpServerDef::Http {
            url: url::Url::parse("https://mcp.example.com/").unwrap(),
            headers: HashMap::from([(
                "Authorization".to_string(),
                Secret::new("cmd:echo bearer-real-secret-token".to_string()),
            )]),
        };
        def.resolve_header_secrets("MCP server", "example").unwrap();

        let McpServerDef::Http { headers, .. } = &def else {
            panic!("expected Http variant");
        };
        assert_eq!(
            headers.get("Authorization").map(|v| v.expose().as_str()),
            Some("bearer-real-secret-token"),
            "header value should resolve to the command output"
        );

        let debug = format!("{def:?}");
        assert!(
            !debug.contains("bearer-real-secret-token"),
            "McpServerDef Debug leaked resolved header value: {debug}"
        );
        assert!(
            debug.contains("redacted"),
            "McpServerDef Debug should mark redaction: {debug}"
        );

        // The guest registration path serializes the resolved def and
        // must emit the real token, not the redacted marker.
        let json = serde_json::to_string(&def).unwrap();
        assert!(
            json.contains("bearer-real-secret-token"),
            "serialized MCP def must carry the real header value for the guest: {json}"
        );
        assert!(
            !json.contains("redacted"),
            "serialized MCP def must not leak the Debug redaction marker: {json}"
        );
    }

    // ── PortForward parsing ──────────────────────────────────

    #[test]
    fn port_forward_parse_guest_only_defaults_host_to_guest() {
        let f = PortForward::parse("3000").unwrap();
        assert_eq!(f.guest.get(), 3000);
        assert_eq!(f.host.get(), 3000);
        assert!(f.label.is_none());
    }

    #[test]
    fn port_forward_parse_guest_host() {
        let f = PortForward::parse("3000:3001").unwrap();
        assert_eq!(f.guest.get(), 3000);
        assert_eq!(f.host.get(), 3001);
    }

    #[test]
    fn port_forward_parse_rejects_zero() {
        assert!(PortForward::parse("0").is_err());
        assert!(PortForward::parse("3000:0").is_err());
    }

    #[test]
    fn port_forward_parse_rejects_non_numeric() {
        assert!(PortForward::parse("abc").is_err());
        assert!(PortForward::parse("3000:abc").is_err());
    }

    #[test]
    fn port_forward_parse_rejects_out_of_range() {
        assert!(PortForward::parse("70000").is_err());
    }

    #[test]
    fn port_forward_toml_integer_form() {
        let toml_src = "forward_ports = [3000]";
        let cfg: CoopConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.forward_ports.len(), 1);
        assert_eq!(cfg.forward_ports[0].guest.get(), 3000);
        assert_eq!(cfg.forward_ports[0].host.get(), 3000);
    }

    #[test]
    fn port_forward_toml_string_form() {
        let toml_src = r#"forward_ports = ["8080:8081"]"#;
        let cfg: CoopConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.forward_ports[0].guest.get(), 8080);
        assert_eq!(cfg.forward_ports[0].host.get(), 8081);
    }

    #[test]
    fn port_forward_toml_table_form() {
        let toml_src = "[[forward_ports]]\nguest = 3000\nhost = 13000\nlabel = \"dev\"\n";
        let cfg: CoopConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.forward_ports[0].guest.get(), 3000);
        assert_eq!(cfg.forward_ports[0].host.get(), 13000);
        assert_eq!(cfg.forward_ports[0].label.as_deref(), Some("dev"));
    }

    #[test]
    fn port_forward_toml_table_omits_host_defaults_to_guest() {
        let toml_src = "[[forward_ports]]\nguest = 3000\n";
        let cfg: CoopConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.forward_ports[0].host.get(), 3000);
    }

    #[test]
    fn port_forward_toml_table_missing_guest_errors() {
        let toml_src = "[[forward_ports]]\nhost = 3000\n";
        let err = match toml::from_str::<CoopConfig>(toml_src) {
            Ok(cfg) => panic!("expected error, got: {cfg:?}"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("guest"), "err = {err}");
    }

    #[test]
    fn port_forward_default_is_empty() {
        let cfg: CoopConfig = toml::from_str("").unwrap();
        assert!(cfg.forward_ports.is_empty());
    }

    #[test]
    fn port_forward_serialize_round_trip() {
        let original = PortForward {
            guest: NonZeroU16::new(3000).unwrap(),
            host: NonZeroU16::new(3001).unwrap(),
            label: Some("dev".to_string()),
        };
        let toml_src = toml::to_string(&original).unwrap();
        let parsed: PortForward = toml::from_str(&toml_src).unwrap();
        assert_eq!(parsed, original);
    }

    // ── PortForward merge ────────────────────────────────────

    fn pf(g: u16, h: u16) -> PortForward {
        PortForward {
            guest: NonZeroU16::new(g).unwrap(),
            host: NonZeroU16::new(h).unwrap(),
            label: None,
        }
    }

    #[test]
    fn merge_forward_ports_appends_cli() {
        let cfg = vec![pf(3000, 3000)];
        let cli = vec![pf(4000, 4000)];
        let merged = merge_forward_ports(&cfg, &cli);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].guest.get(), 3000);
        assert_eq!(merged[1].guest.get(), 4000);
    }

    #[test]
    fn merge_forward_ports_cli_overrides_same_guest() {
        let cfg = vec![pf(3000, 3000)];
        let cli = vec![pf(3000, 13000)];
        let merged = merge_forward_ports(&cfg, &cli);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].host.get(), 13000);
    }

    #[test]
    fn merge_forward_ports_later_cli_overrides_earlier_cli() {
        let cfg: Vec<PortForward> = Vec::new();
        let cli = vec![pf(3000, 13000), pf(3000, 14000)];
        let merged = merge_forward_ports(&cfg, &cli);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].host.get(), 14000);
    }

    // ── Property tests ───────────────────────────────────────
    //
    // A standing `cargo-fuzz` target for the config loader (the parser
    // class #278 reserves fuzzing for) isn't practical here: `CoopConfig`
    // transitively embeds `update`, `setup`, and `shell` types, so the
    // `#[path]`-include trick used by the self-contained `jsonc` /
    // `parse_repo_slug` targets would have to pull in most of the crate
    // (including its network and process-spawning modules). The
    // `config_load_never_panics` property below covers the same
    // "never panics, only returns Err" guarantee as a CI gate; unblocking
    // a true fuzz target would mean giving the crate a `lib` target.

    fn arb_subnet_mask() -> impl Strategy<Value = SubnetMask> {
        (0u8..=32).prop_map(|b| SubnetMask::new(b).unwrap())
    }

    fn arb_host_iface() -> impl Strategy<Value = HostInterface> {
        prop_oneof![
            Just(HostInterface::Auto),
            "[a-z][a-z0-9]{0,8}"
                .prop_map(|s| HostInterface::Named(InterfaceName::new(&s).unwrap())),
        ]
    }

    fn arb_port_forward() -> impl Strategy<Value = PortForward> {
        (
            1u16..=u16::MAX,
            1u16..=u16::MAX,
            proptest::option::of("[a-z]{1,6}"),
        )
            .prop_map(|(guest, host, label)| PortForward {
                guest: NonZeroU16::new(guest).unwrap(),
                host: NonZeroU16::new(host).unwrap(),
                label,
            })
    }

    /// A `CoopConfig` with the drift-prone fields randomized: the numeric
    /// `NonZero`/`Quantity` fields, the custom-serde network fields, the
    /// `forward_ports` list, and an optional `post_start`. Other fields
    /// keep their defaults — enough to exercise serde round-tripping
    /// without an `Arbitrary` impl for every leaf type.
    fn arb_config() -> impl Strategy<Value = CoopConfig> {
        (
            // Guest memory must stay at or above the floor: `VmMemory`
            // rejects anything below `MIN_MEM_MIB` at deserialize, so a
            // lower value would fail the serde round-trip this drives.
            MIN_MEM_MIB.as_u32()..=u32::MAX,
            1u8..=u8::MAX,
            1u32..=u32::MAX,
            arb_subnet_mask(),
            arb_host_iface(),
            proptest::collection::vec(arb_port_forward(), 0..4),
            proptest::option::of("[a-zA-Z0-9 _./:-]{0,20}"),
        )
            .prop_map(
                |(mem, vcpu, template, subnet_mask, host_iface, forward_ports, post_start)| {
                    let mut cfg = CoopConfig::default();
                    cfg.vm.mem_size_mib = VmMemory::new(MiB::new(mem).unwrap()).unwrap();
                    cfg.vm.vcpu_count = NonZeroU8::new(vcpu).unwrap();
                    cfg.vm.template_size_gib = GiB::new(template).unwrap();
                    cfg.network.subnet_mask = subnet_mask;
                    cfg.network.host_iface = host_iface;
                    cfg.forward_ports = forward_ports;
                    cfg.post_start = post_start;
                    cfg
                },
            )
    }

    /// A TOML document biased toward real keys and adversarial scalar
    /// values, so the loader's custom deserializers are actually reached
    /// rather than rejected by the lexer. Most documents fail to parse;
    /// the property is only that none panic.
    fn arb_config_toml_text() -> impl Strategy<Value = String> {
        let scalar = prop_oneof![
            any::<i64>().prop_map(|n| n.to_string()),
            Just("\"/24\"".to_string()),
            Just("\"auto\"".to_string()),
            Just("\" auto\"".to_string()),
            Just("\"eth/0\"".to_string()),
            Just("0".to_string()),
            Just("[3000, \"8080:8081\"]".to_string()),
            Just("{ guest = 3000, host = 0 }".to_string()),
            "[a-zA-Z0-9_./-]{0,10}".prop_map(|s| format!("\"{s}\"")),
        ];
        let key = proptest::sample::select(vec![
            "mem_size_mib",
            "vcpu_count",
            "ssh_port",
            "subnet_mask",
            "host_iface",
            "host_ip",
            "data_dir",
            "post_start",
            "template_size_gib",
            "forward_ports",
        ]);
        let header = proptest::sample::select(vec![
            "",
            "[vm]",
            "[network]",
            "[claude]",
            "[[forward_ports]]",
        ]);
        proptest::collection::vec((header, key, scalar), 0..8).prop_map(|rows| {
            let mut out = String::new();
            for (header, key, scalar) in rows {
                if !header.is_empty() {
                    out.push_str(header);
                    out.push('\n');
                }
                out.push_str(key);
                out.push_str(" = ");
                out.push_str(&scalar);
                out.push('\n');
            }
            out
        })
    }

    fn arb_guest_path() -> impl Strategy<Value = String> {
        // Absolute, colon-free guest path so `Mount`'s `HOST:GUEST` split
        // and `GuestPath::absolute` both accept it.
        proptest::collection::vec("[a-zA-Z0-9_.-]{1,8}", 1..4)
            .prop_map(|segments| format!("/{}", segments.join("/")))
    }

    proptest! {
        /// `Quantity::parse_cli` → `Display` → `parse_cli` is the identity:
        /// `Display` writes the bare integer and `parse_cli` reads it back,
        /// for both unit markers.
        #[test]
        fn quantity_parse_display_roundtrips(n in 1u32..=u32::MAX) {
            let mib = MiB::parse_cli(&n.to_string()).unwrap();
            prop_assert_eq!(mib.as_u32(), n);
            prop_assert_eq!(MiB::parse_cli(&mib.to_string()).unwrap(), mib);

            let gib = GiB::parse_cli(&n.to_string()).unwrap();
            prop_assert_eq!(gib.as_u32(), n);
            prop_assert_eq!(GiB::parse_cli(&gib.to_string()).unwrap(), gib);
        }

        /// `DiskSize` has no `Display`, so its canonical string is rebuilt
        /// from the parsed variant (`N` absolute, `+N` relative) and
        /// re-parsed. Pins that both the `+` prefix and the GiB magnitude
        /// survive a round-trip.
        #[test]
        fn disk_size_parse_render_roundtrips(relative in any::<bool>(), n in 1u32..=u32::MAX) {
            let spec = if relative { format!("+{n}") } else { n.to_string() };
            let ds = DiskSize::parse(&spec).unwrap();
            let canonical = match ds {
                DiskSize::Absolute(gib) => gib.to_string(),
                DiskSize::Relative(gib) => format!("+{gib}"),
            };
            prop_assert_eq!(DiskSize::parse(&canonical).unwrap(), ds);
        }

        /// `PortForward::parse` of a `GUEST[:HOST]` spec round-trips through
        /// the canonical `guest:host` rendering. CLI parsing never sets a
        /// label, so value equality covers the whole struct.
        #[test]
        fn port_forward_parse_render_roundtrips(
            guest in 1u16..=u16::MAX,
            host in proptest::option::of(1u16..=u16::MAX),
        ) {
            let spec = match host {
                Some(h) => format!("{guest}:{h}"),
                None => guest.to_string(),
            };
            let parsed = PortForward::parse(&spec).unwrap();
            let canonical = format!("{}:{}", parsed.guest, parsed.host);
            prop_assert_eq!(PortForward::parse(&canonical).unwrap(), parsed);
        }

        /// `SubnetMask` round-trips `Display` → `FromStr` across the whole
        /// `0..=32` range. (The leading `/` itself is pinned by the serde
        /// JSON test, since `FromStr` also accepts the bare form.)
        #[test]
        fn subnet_mask_display_fromstr_roundtrips(bits in 0u8..=32) {
            let mask = SubnetMask::new(bits).unwrap();
            prop_assert_eq!(mask.to_string().parse::<SubnetMask>().unwrap(), mask);
        }

        /// `Mount::parse` round-trips through `HOST:GUEST`: re-parsing the
        /// canonical spec rebuilt from a parsed mount yields the same
        /// (canonicalized) host and guest paths. The host is a real temp
        /// dir so the existence/is-dir invariants hold.
        #[test]
        fn mount_parse_roundtrips(guest in arb_guest_path()) {
            let tmp = TempDir::new().unwrap();
            let host = tmp.path().to_str().unwrap();
            let first = Mount::parse(&format!("{host}:{guest}")).unwrap();
            let second =
                Mount::parse(&format!("{}:{}", first.host_path.display(), first.guest_path))
                    .unwrap();
            prop_assert_eq!(&first.host_path, &second.host_path);
            prop_assert_eq!(first.guest_path.to_string(), second.guest_path.to_string());
        }

        /// Loading a serialized config and re-serializing is idempotent:
        /// `serialize(load(serialize(cfg))) == serialize(cfg)`. Catches
        /// serde default/skip/rename drift and any custom (de)serialize
        /// impl that fails to round-trip (`SubnetMask`, `HostInterface`,
        /// `PortForward`). No prior test covered the whole struct.
        #[test]
        fn config_toml_roundtrip_is_idempotent(cfg in arb_config()) {
            let once = toml::to_string(&cfg).unwrap();
            let parsed: CoopConfig = toml::from_str(&once).unwrap();
            let twice = toml::to_string(&parsed).unwrap();
            prop_assert_eq!(once, twice);
        }

        /// The loader must never panic on hostile input — only `Ok`/`Err`.
        /// `proptest` reports any panic as a failure with a minimized case.
        #[test]
        fn config_load_never_panics(src in arb_config_toml_text()) {
            let _ = toml::from_str::<CoopConfig>(&src);
        }

        /// `merge_forward_ports` is keyed on the guest port: the result
        /// holds each distinct guest exactly once, in first-seen order,
        /// and the surviving entry is the LAST across `config ++ cli`
        /// (CLI overrides config; a later duplicate overrides an earlier
        /// one). Generalizes the three example-based merge tests above.
        #[test]
        fn merge_forward_ports_keeps_last_per_guest(
            config in proptest::collection::vec(arb_port_forward(), 0..6),
            cli in proptest::collection::vec(arb_port_forward(), 0..6),
        ) {
            let merged = merge_forward_ports(&config, &cli);

            let mut order: Vec<NonZeroU16> = Vec::new();
            let mut last: HashMap<NonZeroU16, PortForward> = HashMap::new();
            for f in config.iter().chain(cli.iter()) {
                if last.insert(f.guest, f.clone()).is_none() {
                    order.push(f.guest);
                }
            }

            prop_assert_eq!(merged.len(), order.len());
            for (got, guest) in merged.iter().zip(order.iter()) {
                prop_assert_eq!(&got.guest, guest);
                prop_assert_eq!(got, &last[guest]);
            }
        }
    }
}
