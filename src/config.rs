use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::net::Ipv4Addr;
use std::num::{NonZeroU8, NonZeroU16, NonZeroU32};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cmd::Cmd;

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

/// Memory size in mebibytes. Inner `NonZeroU32` rejects zero at
/// deserialization time — no runtime validation needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MiB(NonZeroU32);

impl MiB {
    /// Create from a runtime value. Returns `None` if zero.
    pub fn new(value: u32) -> Option<Self> {
        NonZeroU32::new(value).map(Self)
    }

    pub fn as_u32(self) -> u32 {
        self.0.get()
    }

    pub fn as_gib_f64(self) -> f64 {
        f64::from(self.0.get()) / 1024.0
    }
}

impl fmt::Display for MiB {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Disk size in gibibytes. Inner `NonZeroU32` rejects zero at
/// deserialization time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GiB(NonZeroU32);

impl GiB {
    /// Create from a runtime value. Returns `None` if zero.
    pub fn new(value: u32) -> Option<Self> {
        NonZeroU32::new(value).map(Self)
    }

    pub fn as_u32(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for GiB {
    #[mutants::skip] // equivalent: callers don't assert the formatted output, only that GiB round-trips through CLI parsing
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
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
    pub fn resolve(self, current_gib: u32) -> Result<GiB> {
        let target = match self {
            Self::Absolute(gib) => gib.as_u32(),
            Self::Relative(gib) => current_gib
                .checked_add(gib.as_u32())
                .context("Disk size overflow")?,
        };
        GiB::new(target).context("Resolved disk size is 0")
    }
}

/// Instance index (0..=252), used to derive guest IP, TAP name, MAC, and vsock CID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceIndex(u16);

impl InstanceIndex {
    pub fn new(value: u16) -> Self {
        Self(value)
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

/// A host directory to mount into the guest VM.
#[derive(Debug, Clone)]
pub struct Mount {
    pub host_path: PathBuf,
    pub guest_path: String,
}

impl Mount {
    /// Parse a mount spec in the form `HOST_PATH[:GUEST_PATH]`.
    ///
    /// If `GUEST_PATH` is omitted, defaults to `/mnt/<dirname>` where
    /// `<dirname>` is the last component of the host path.
    pub fn parse(spec: &str) -> Result<Self> {
        let (host, guest_path) = if let Some((h, g)) = spec.split_once(':') {
            (h, g.to_string())
        } else {
            (spec, "/workspace".to_string())
        };

        let host_path = Path::new(host)
            .canonicalize()
            .with_context(|| format!("Mount host path does not exist: {host}"))?;

        anyhow::ensure!(
            host_path.is_dir(),
            "Mount host path is not a directory: {}",
            host_path.display()
        );

        anyhow::ensure!(
            guest_path.starts_with('/'),
            "Mount guest path must be absolute: {guest_path}"
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

#[derive(Debug, Serialize, Deserialize)]
pub struct CoopConfig {
    /// Directory for storing VM artifacts (images, sockets, logs)
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    #[serde(default)]
    pub vm: VmConfig,
    #[serde(default)]
    pub network: NetworkConfig,

    /// SSH port on the guest
    #[serde(default = "default_ssh_port")]
    pub ssh_port: NonZeroU16,

    /// Path to firecracker binary
    #[serde(default = "default_firecracker_bin")]
    pub firecracker_bin: PathBuf,

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
    pub guest_env: BTreeMap<String, String>,

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

    /// Default host:guest port forwards applied to every `coop start`.
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

    /// Memory size in MiB
    #[serde(default = "default_mem_mib")]
    pub mem_size_mib: MiB,

    /// Path to vmlinux kernel image
    #[serde(default = "default_kernel_path")]
    pub kernel_path: PathBuf,

    /// Kernel boot arguments
    #[serde(default = "default_boot_args")]
    pub boot_args: String,

    /// Template rootfs size in GiB (used during setup)
    #[serde(default = "default_template_size_gib")]
    pub template_size_gib: GiB,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Host IP on TAP interfaces
    #[serde(default = "default_host_ip")]
    pub host_ip: Ipv4Addr,

    /// Subnet mask in CIDR notation
    #[serde(default = "default_subnet_mask")]
    pub subnet_mask: String,

    /// Host network interface for NAT (e.g., eth0, ens5)
    #[serde(default = "default_host_iface")]
    pub host_iface: String,
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
    pub fn pat_entry(&self, repo: &str) -> Option<&PatEntry> {
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
    #[serde(default, rename = "pat")]
    pub entries: std::collections::BTreeMap<String, PatEntry>,
    /// Repos for which the auto-prompt at `coop start` is suppressed.
    #[serde(default)]
    pub skip: Vec<String>,
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
                let mut entries: Option<std::collections::BTreeMap<String, PatEntry>> = None;
                let mut skip: Option<Vec<String>> = None;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerDef {
    /// Command for stdio servers
    pub command: Option<String>,
    /// Arguments for stdio servers
    #[serde(default)]
    pub args: Vec<String>,

    /// Server type ("http", "sse"); maps to `type` in Claude Code CLI
    #[serde(rename = "type")]
    pub server_type: Option<String>,
    /// URL for HTTP servers
    pub url: Option<String>,

    /// Env var name mappings (key = server env name, value = host env var name)
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP headers
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConfigDir {
    #[default]
    Default,
    Custom(PathBuf),
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
                Ok(ConfigDir::Custom(PathBuf::from(v)))
            }

            #[mutants::skip] // equivalent: serde routes owned strings through visit_str; this path isn't exercised by our deserializer
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(ConfigDir::Custom(PathBuf::from(v)))
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
    pub env_forward: Vec<String>,

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
}

/// `[setup]` section: controls one-time UX behaviour at `coop start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupConfig {
    /// Whether `coop start` prompts the user to set up a fine-grained PAT
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
    pub env_forward: Vec<String>,

    /// MCP servers to register in `~/.codex/config.toml`
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerDef>,

    /// Source directory for Codex config files (config.toml, AGENTS.md, prompts/)
    #[serde(default)]
    pub config_dir: ConfigDir,
}

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
        bail!(
            "Instance name contains invalid character '{c}' \
             (allowed: a-z, A-Z, 0-9, '-', '_')"
        );
    }
    Ok(())
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
        if let ConfigDir::Custom(ref path) = cfg.claude.config_dir
            && path.starts_with("~")
        {
            cfg.claude.config_dir = ConfigDir::Custom(crate::shell::expand_tilde(path));
        }
        if let ConfigDir::Custom(ref path) = cfg.codex.config_dir
            && path.starts_with("~")
        {
            cfg.codex.config_dir = ConfigDir::Custom(crate::shell::expand_tilde(path));
        }
        cfg.claude.marketplaces = cfg
            .claude
            .marketplaces
            .into_iter()
            .map(|s| {
                if s.starts_with('~') {
                    crate::shell::expand_tilde(Path::new(&s))
                        .to_string_lossy()
                        .into_owned()
                } else {
                    s
                }
            })
            .collect();
        Ok(cfg)
    }

    /// Validate config values, returning all problems found.
    ///
    /// Checks numeric bounds, IP/CIDR parsing, and path accessibility.
    /// Returns `Ok(warnings)` where warnings are non-fatal observations,
    /// or `Err` with all fatal validation errors joined.
    pub fn validate(&self) -> Result<Vec<String>> {
        let mut errors: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // VM config bounds
        // vcpu_count, mem_size_mib, template_size_gib, and ssh_port are
        // NonZero types — zero is rejected at deserialization time.
        if self.vm.mem_size_mib.as_u32() < 128 {
            errors.push(format!(
                "vm.mem_size_mib={} is too low (minimum 128)",
                self.vm.mem_size_mib
            ));
        }

        // Network validation
        let mask = self.network.subnet_mask.trim_start_matches('/');
        match mask.parse::<u8>() {
            Ok(bits) if bits <= 32 => {}
            _ => errors.push(format!(
                "network.subnet_mask='{}' is not valid CIDR (expected /0../32)",
                self.network.subnet_mask
            )),
        }

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

        for mp in &self.claude.marketplaces {
            let path = Path::new(mp);
            if path.is_absolute() && !path.exists() {
                errors.push(format!(
                    "claude.marketplaces entry '{mp}' looks like a local \
                     path but does not exist"
                ));
            }
        }

        if let Some(GitHubAuth::Pat(pat)) = self.github.as_ref() {
            for key in pat.entries.keys() {
                if let Err(e) = crate::github_repo::validate_repo_slug(key) {
                    errors.push(format!(
                        "github.pat.\"{key}\" key is not a valid 'owner/repo' slug: {e}"
                    ));
                }
            }
            for slug in &pat.skip {
                if let Err(e) = crate::github_repo::validate_repo_slug(slug) {
                    errors.push(format!(
                        "github.skip entry '{slug}' is not a valid 'owner/repo' slug: {e}"
                    ));
                }
            }
        }

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
    pub fn image_dir(&self, name: &str) -> PathBuf {
        self.images_dir().join(name)
    }

    /// Path to the template rootfs image for a named image.
    pub fn template_path_for(&self, image: &str) -> PathBuf {
        self.image_dir(image).join("rootfs-template.ext4")
    }

    /// Path to the template config for a named image.
    pub fn template_config_path_for(&self, image: &str) -> PathBuf {
        self.image_dir(image).join("template-config.json")
    }

    /// Path to the Lima base image for a named image.
    pub fn lima_base_path(&self, image: &str) -> PathBuf {
        self.image_dir(image).join("lima-base.img")
    }

    /// Path to the Lima start template for a named image.
    pub fn lima_template_path(&self, image: &str) -> PathBuf {
        self.image_dir(image).join("lima-template.yaml")
    }

    /// Path to the default template rootfs image (shorthand).
    pub fn template_path(&self) -> PathBuf {
        self.template_path_for(DEFAULT_IMAGE)
    }

    /// List all available images with their metadata.
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
            let name = entry.file_name().to_string_lossy().to_string();
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
        images.sort_by(|a, b| a.name.cmp(&b.name));
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
    pub fn resolve_instance(&self, name: Option<&str>) -> Result<Instance> {
        let instances = self.list_instances()?;
        if let Some(name) = name {
            instances
                .into_iter()
                .find(|i| i.name == *name)
                .with_context(|| {
                    let available = self.format_instance_list_or_none();
                    format!(
                        "No instance named '{name}'. {available}\n\
                         Create one with: coop start --name {name}"
                    )
                })
        } else if instances.len() == 1 {
            // Safe: we just checked len == 1
            instances
                .into_iter()
                .next()
                .context("Instance list unexpectedly empty")
        } else if instances.is_empty() {
            bail!(
                "No instances found.\n\
                 Create one with: coop start\n\
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
        name: Option<&str>,
        image: &str,
        workspace_path: Option<&Path>,
    ) -> Result<Instance> {
        const MAX_INDEX: u16 = 252;

        let _lock = lock_dir(&self.instances_dir())?;

        let instances = self.list_instances()?;
        let used_indices: HashSet<InstanceIndex> = instances.iter().map(|i| i.index).collect();

        // Start from highest + 1, then fall back to lowest gap
        let highest = instances.iter().map(|i| i.index.as_u16()).max();
        let raw_index = match highest {
            Some(h) if h < MAX_INDEX && !used_indices.contains(&InstanceIndex::new(h + 1)) => h + 1,
            _ => (0..=MAX_INDEX)
                .find(|i| !used_indices.contains(&InstanceIndex::new(*i)))
                .context("All 253 instance slots are in use")?,
        };
        let index = InstanceIndex::new(raw_index);

        let name = if let Some(n) = name {
            InstanceName::new(n)?
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
            image: image.to_string(),
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
        }
    }
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            api_key: std::env::var("OPENAI_API_KEY").ok().map(Secret::new),
            env_forward: Vec::new(),
            mcp_servers: HashMap::new(),
            config_dir: ConfigDir::Default,
        }
    }
}

// ── Image info ────────────────────────────────────────────────

/// Metadata about a named golden image.
pub struct ImageInfo {
    pub name: String,
    pub dir: PathBuf,
    pub config: Option<crate::setup::TemplateConfig>,
}

// ── Instance ──────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct InstanceMeta {
    name: InstanceName,
    index: InstanceIndex,
    #[serde(default = "default_image_name")]
    image: String,
}

fn default_image_name() -> String {
    DEFAULT_IMAGE.to_string()
}

#[derive(Debug)]
pub struct Instance {
    pub name: InstanceName,
    pub index: InstanceIndex,
    pub dir: PathBuf,
    /// Name of the golden image this instance was created from.
    pub image: String,
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

    pub fn tap_device(&self) -> String {
        format!("tap{}", self.index)
    }

    pub fn guest_ip(&self) -> String {
        format!("172.16.0.{}", self.index.as_u32() + 2)
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

fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".coop")
}

fn default_vcpus() -> NonZeroU8 {
    #[expect(clippy::expect_used, reason = "literal is provably non-zero")]
    NonZeroU8::new(2).expect("2 is non-zero")
}

fn default_mem_mib() -> MiB {
    #[expect(clippy::expect_used, reason = "literal is provably non-zero")]
    MiB::new(4096).expect("4096 is non-zero")
}

fn default_kernel_path() -> PathBuf {
    default_data_dir().join("vmlinux")
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

fn default_subnet_mask() -> String {
    "/24".to_string()
}

#[mutants::skip] // equivalent: "auto" is consumed by host-interface auto-detection at runtime, not by any unit test
fn default_host_iface() -> String {
    "auto".to_string()
}

fn default_ssh_port() -> NonZeroU16 {
    #[expect(clippy::expect_used, reason = "literal is provably non-zero")]
    NonZeroU16::new(22).expect("22 is non-zero")
}

fn default_firecracker_bin() -> PathBuf {
    default_data_dir().join("firecracker")
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
#[expect(clippy::panic, reason = "tests use panic! for unreachable arms")]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> CoopConfig {
        CoopConfig {
            data_dir: tmp.path().to_path_buf(),
            ..CoopConfig::default()
        }
    }

    fn make_instance(dir: &Path, name: &str, index: u16) -> Instance {
        let inst = Instance {
            name: InstanceName::new(name).unwrap(),
            index: InstanceIndex::new(index),
            dir: dir.join("instances").join(name),
            image: DEFAULT_IMAGE.to_string(),
        };
        inst.save().unwrap();
        inst
    }

    fn test_inst(name: &str, index: u16, dir: PathBuf) -> Instance {
        Instance {
            name: InstanceName::new(name).unwrap(),
            index: InstanceIndex::new(index),
            dir,
            image: DEFAULT_IMAGE.to_string(),
        }
    }

    // ── Instance network derivation ──────────────────────────

    #[test]
    fn instance_guest_ip_from_index() {
        let inst = test_inst("test", 0, PathBuf::from("/tmp/fake"));
        assert_eq!(inst.guest_ip(), "172.16.0.2");

        let inst = test_inst("test", 252, PathBuf::from("/tmp/fake"));
        assert_eq!(inst.guest_ip(), "172.16.0.254");
    }

    #[test]
    fn instance_tap_device_from_index() {
        let inst = test_inst("test", 5, PathBuf::from("/tmp/fake"));
        assert_eq!(inst.tap_device(), "tap5");
    }

    #[test]
    fn instance_mac_from_index() {
        let inst = test_inst("test", 0, PathBuf::from("/tmp/fake"));
        assert_eq!(inst.guest_mac(), "06:00:AC:10:00:02");

        let inst = test_inst("test", 252, PathBuf::from("/tmp/fake"));
        assert_eq!(inst.guest_mac(), "06:00:AC:10:00:fe");
    }

    #[test]
    fn instance_vsock_cid_from_index() {
        let inst = test_inst("test", 0, PathBuf::from("/tmp/fake"));
        assert_eq!(inst.vsock_cid(), 3);

        let inst = test_inst("test", 10, PathBuf::from("/tmp/fake"));
        assert_eq!(inst.vsock_cid(), 13);
    }

    // ── Instance paths ───────────────────────────────────────

    #[test]
    fn instance_paths_under_dir() {
        let inst = test_inst("foo", 0, PathBuf::from("/data/instances/foo"));
        assert_eq!(
            inst.rootfs_path(),
            PathBuf::from("/data/instances/foo/rootfs.ext4")
        );
        assert_eq!(
            inst.pid_file_path(),
            PathBuf::from("/data/instances/foo/firecracker.pid")
        );
        assert_eq!(
            inst.api_socket_path(),
            PathBuf::from("/data/instances/foo/firecracker.socket")
        );
        assert_eq!(
            inst.log_path(),
            PathBuf::from("/data/instances/foo/firecracker.log")
        );
        assert_eq!(
            inst.vsock_path(),
            PathBuf::from("/data/instances/foo/vsock.sock")
        );
        assert_eq!(
            inst.vm_config_path(),
            PathBuf::from("/data/instances/foo/vm_config.json")
        );
        assert_eq!(
            inst.forwards_state_path(),
            PathBuf::from("/data/instances/foo/forwards.json")
        );
    }

    // ── Instance save/load roundtrip ─────────────────────────

    #[test]
    fn instance_save_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("myinst");
        let inst = test_inst("myinst", 42, dir.clone());
        inst.save().unwrap();

        let loaded = Instance::load(&dir).unwrap();
        assert_eq!(loaded.name, *"myinst");
        assert_eq!(loaded.index, InstanceIndex::new(42));
        assert_eq!(loaded.dir, dir);
        assert_eq!(loaded.image, DEFAULT_IMAGE);
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
        let inst = test_inst("test", 0, tmp.path().to_path_buf());
        assert!(!inst.pid_file_path().exists());
        assert!(!inst.is_running());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn is_running_false_for_dead_pid_and_removes_pid_file() {
        let tmp = TempDir::new().unwrap();
        let inst = test_inst("test", 0, tmp.path().to_path_buf());
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
        let inst = test_inst("test", 0, tmp.path().to_path_buf());
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
        let inst = test_inst("test", 0, tmp.path().to_path_buf());
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
        let inst = cfg.allocate_instance(None, DEFAULT_IMAGE, None).unwrap();
        assert_eq!(inst.index, InstanceIndex::new(0));
        assert_eq!(inst.name, *"0");
    }

    #[test]
    fn allocate_sequential_instances() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let a = cfg.allocate_instance(None, DEFAULT_IMAGE, None).unwrap();
        let b = cfg.allocate_instance(None, DEFAULT_IMAGE, None).unwrap();
        let c = cfg.allocate_instance(None, DEFAULT_IMAGE, None).unwrap();

        assert_eq!(a.index, InstanceIndex::new(0));
        assert_eq!(b.index, InstanceIndex::new(1));
        assert_eq!(c.index, InstanceIndex::new(2));
    }

    #[test]
    fn allocate_with_custom_name() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let inst = cfg
            .allocate_instance(Some("my-project"), DEFAULT_IMAGE, None)
            .unwrap();
        assert_eq!(inst.name, *"my-project");
        assert_eq!(inst.index, InstanceIndex::new(0));
    }

    #[test]
    fn allocate_rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        cfg.allocate_instance(Some("dupe"), DEFAULT_IMAGE, None)
            .unwrap();
        let err = cfg
            .allocate_instance(Some("dupe"), DEFAULT_IMAGE, None)
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn allocate_continues_after_highest() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        // Create instance at index 0, then remove it, then create at 1
        let inst0 = cfg
            .allocate_instance(Some("a"), DEFAULT_IMAGE, None)
            .unwrap();
        assert_eq!(inst0.index, InstanceIndex::new(0));
        let inst1 = cfg
            .allocate_instance(Some("b"), DEFAULT_IMAGE, None)
            .unwrap();
        assert_eq!(inst1.index, InstanceIndex::new(1));

        // Remove instance 0 by deleting its dir
        fs::remove_dir_all(&inst0.dir).unwrap();

        // Next allocation should be index 2 (highest + 1), not 0 (gap)
        let inst2 = cfg
            .allocate_instance(Some("c"), DEFAULT_IMAGE, None)
            .unwrap();
        assert_eq!(inst2.index, InstanceIndex::new(2));
    }

    #[test]
    fn allocate_fills_gap_when_at_ceiling() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        // Create instance at index 252 (max)
        make_instance(tmp.path(), "max", 252);

        // Create another at index 0 (gap at low end)
        make_instance(tmp.path(), "zero", 0);

        // Remove index 0
        fs::remove_dir_all(tmp.path().join("instances/zero")).unwrap();

        // Next should fill gap at 0 since highest (252) is at ceiling
        let inst = cfg
            .allocate_instance(Some("fill"), DEFAULT_IMAGE, None)
            .unwrap();
        assert_eq!(inst.index, InstanceIndex::new(0));
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

    #[test]
    fn allocate_rejects_invalid_name() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        let err = cfg
            .allocate_instance(Some("../evil"), DEFAULT_IMAGE, None)
            .unwrap_err();
        assert!(err.to_string().contains("invalid character"));
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

        make_instance(tmp.path(), "high", 10);
        make_instance(tmp.path(), "low", 2);
        make_instance(tmp.path(), "mid", 5);

        let instances = cfg.list_instances().unwrap();
        let indices: Vec<InstanceIndex> = instances.iter().map(|i| i.index).collect();
        assert_eq!(
            indices,
            vec![
                InstanceIndex::new(2),
                InstanceIndex::new(5),
                InstanceIndex::new(10)
            ]
        );
    }

    // ── Resolve instance ─────────────────────────────────────

    #[test]
    fn resolve_auto_selects_single() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "only", 0);

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

        make_instance(tmp.path(), "a", 0);
        make_instance(tmp.path(), "b", 1);

        let err = cfg.resolve_instance(None).unwrap_err();
        assert!(err.to_string().contains("Multiple instances"));
    }

    #[test]
    fn resolve_by_name() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "alpha", 0);
        make_instance(tmp.path(), "beta", 1);

        let inst = cfg.resolve_instance(Some("beta")).unwrap();
        assert_eq!(inst.name, *"beta");
        assert_eq!(inst.index, InstanceIndex::new(1));
    }

    #[test]
    fn resolve_unknown_name_errors() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "real", 0);

        let err = cfg.resolve_instance(Some("fake")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No instance named 'fake'"));
        assert!(msg.contains("Available: real"), "missing hint in: {msg}");
    }

    #[test]
    fn resolve_unknown_name_with_no_instances_lists_none() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        let err = cfg.resolve_instance(Some("ghost")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No instance named 'ghost'"));
        assert!(
            msg.contains("No instances exist."),
            "missing hint in: {msg}"
        );
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
        make_instance(tmp.path(), "alpha", 0);
        make_instance(tmp.path(), "beta", 1);
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
        assert_eq!(cfg.env_forward, vec!["MYORG_KEY"]);
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
        assert_eq!(cfg.env_forward, vec!["MYORG_KEY"]);
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert!(cfg.mcp_servers.contains_key("sentry"));
    }

    #[test]
    fn codex_config_all_defaults() {
        let json = "{}";
        let cfg: CodexConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.api_key.is_none());
        assert!(cfg.env_forward.is_empty());
        assert!(cfg.mcp_servers.is_empty());
        assert_eq!(cfg.config_dir, ConfigDir::Default);
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
        assert_eq!(
            pat.entries
                .get("trailofbits/coop")
                .map(|e| e.token.expose().as_str()),
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
        assert_eq!(pat.skip, vec!["a/b".to_string()]);
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
        let entry = auth.pat_entry("a/b").unwrap();
        assert_eq!(entry.token.expose(), "cmd:echo x");
        assert!(auth.pat_entry("c/d").is_none());
    }

    #[test]
    fn github_auth_lookup_returns_none_for_non_pat_modes() {
        let auth = GitHubAuth::Auto;
        assert!(auth.pat_entry("a/b").is_none());
        let auth = GitHubAuth::Env;
        assert!(auth.pat_entry("a/b").is_none());
        let auth = GitHubAuth::Off;
        assert!(auth.pat_entry("a/b").is_none());
    }

    #[test]
    fn github_auth_serializes_round_trip_for_pat() {
        // pat-mode → table form; deserialize the serialized output and
        // confirm the entries survived.
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "a/b".to_string(),
            PatEntry {
                token: Secret::new("cmd:echo x".to_string()),
            },
        );
        let auth = GitHubAuth::Pat(PatConfig {
            entries,
            skip: vec!["c/d".to_string()],
        });
        let serialized = toml::to_string(&auth).unwrap();
        let parsed: GitHubAuth = toml::from_str(&serialized).unwrap();
        let pat = match parsed {
            GitHubAuth::Pat(p) => p,
            other => panic!("expected Pat variant after round-trip, got {other:?}"),
        };
        assert_eq!(pat.skip, vec!["c/d".to_string()]);
        assert_eq!(
            pat.entries.get("a/b").map(|e| e.token.expose().as_str()),
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
        assert_eq!(def.command.as_deref(), Some("npx"));
        assert_eq!(def.args, vec!["-y", "@myorg/mcp-server"]);
        assert_eq!(
            def.env.get("API_KEY").map(String::as_str),
            Some("MYORG_API_KEY")
        );
        assert!(def.server_type.is_none());
        assert!(def.url.is_none());
    }

    #[test]
    fn mcp_server_http_def() {
        let json = r#"{
            "type": "http",
            "url": "https://mcp.sentry.dev/mcp"
        }"#;
        let def: McpServerDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.server_type.as_deref(), Some("http"));
        assert_eq!(def.url.as_deref(), Some("https://mcp.sentry.dev/mcp"));
        assert!(def.command.is_none());
        assert!(def.args.is_empty());
    }

    // ── Config loading ────────────────────────────────────────

    #[test]
    fn load_missing_file_returns_defaults() {
        let cfg = CoopConfig::load(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(cfg.vm.vcpu_count.get(), 2);
        assert_eq!(cfg.vm.mem_size_mib, MiB::new(4096).unwrap());
        assert_eq!(cfg.ssh_port.get(), 22);
        assert_eq!(cfg.network.host_ip, Ipv4Addr::new(172, 16, 0, 1));
        assert_eq!(cfg.network.subnet_mask, "/24");
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
    fn load_valid_toml_parses() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "ssh_port = 2222\n\n[vm]\nvcpu_count = 4\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(cfg.ssh_port.get(), 2222);
        assert_eq!(cfg.vm.vcpu_count.get(), 4);
        // Unspecified fields get defaults
        assert_eq!(cfg.vm.mem_size_mib, MiB::new(4096).unwrap());
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
        assert_eq!(cfg.vm.mem_size_mib, MiB::new(8192).unwrap());
        assert_eq!(cfg.vm.vcpu_count.get(), 2);
        assert_eq!(cfg.vm.template_size_gib, GiB::new(8).unwrap());
    }

    #[test]
    fn load_partial_network_config_fills_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[network]\nhost_iface = \"eth0\"\n").unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(cfg.network.host_iface, "eth0");
        assert_eq!(cfg.network.host_ip, Ipv4Addr::new(172, 16, 0, 1));
        assert_eq!(cfg.network.subnet_mask, "/24");
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

    // ── Config path construction ──────────────────────────────

    #[test]
    fn config_paths_relative_to_data_dir() {
        let cfg = CoopConfig {
            data_dir: PathBuf::from("/my/data"),
            ..CoopConfig::default()
        };
        // Default image paths
        assert_eq!(
            cfg.template_path(),
            PathBuf::from("/my/data/images/default/rootfs-template.ext4")
        );
        assert_eq!(
            cfg.template_config_path_for(DEFAULT_IMAGE),
            PathBuf::from("/my/data/images/default/template-config.json")
        );
        // Named image paths
        assert_eq!(
            cfg.template_path_for("python-dev"),
            PathBuf::from("/my/data/images/python-dev/rootfs-template.ext4")
        );
        assert_eq!(
            cfg.lima_base_path("python-dev"),
            PathBuf::from("/my/data/images/python-dev/lima-base.img")
        );
        assert_eq!(cfg.ssh_key_path(), PathBuf::from("/my/data/vm_key"));
        assert_eq!(cfg.instances_dir(), PathBuf::from("/my/data/instances"));
        assert_eq!(cfg.images_dir(), PathBuf::from("/my/data/images"));
    }

    // ── Default values ────────────────────────────────────────

    #[test]
    fn default_data_dir_is_under_home() {
        let dir = default_data_dir();
        assert!(
            dir.ends_with(".coop"),
            "expected path ending with .coop, got: {dir:?}"
        );
    }

    #[test]
    fn default_kernel_path_is_under_data_dir() {
        let kernel = default_kernel_path();
        let data = default_data_dir();
        assert_eq!(kernel, data.join("vmlinux"));
    }

    #[test]
    fn default_firecracker_bin_is_under_data_dir() {
        let bin = default_firecracker_bin();
        let data = default_data_dir();
        assert_eq!(bin, data.join("firecracker"));
    }

    // ── Config validation ─────────────────────────────────────

    #[test]
    fn validate_defaults_pass() {
        let cfg = CoopConfig::default();
        let warnings = cfg.validate().unwrap();
        // Warnings are acceptable; errors are not
        assert!(warnings.len() <= 3, "unexpected warnings: {warnings:?}");
    }

    // Zero-value rejection is now enforced by NonZero types at
    // deserialization time (tested in the "NonZero type enforcement"
    // section above). The validate() method only checks semantic
    // bounds like mem_size_mib >= 128.

    #[test]
    fn validate_rejects_low_memory() {
        let mut cfg = CoopConfig::default();
        cfg.vm.mem_size_mib = MiB::new(64).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("mem_size_mib=64 is too low"));
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
    fn validate_rejects_invalid_subnet_mask() {
        let mut cfg = CoopConfig::default();
        cfg.network.subnet_mask = "/33".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("not valid CIDR"));
    }

    #[test]
    fn validate_rejects_non_cidr_subnet() {
        let mut cfg = CoopConfig::default();
        cfg.network.subnet_mask = "255.255.255.0".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("not valid CIDR"));
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
    fn validate_collects_multiple_errors() {
        let mut cfg = CoopConfig::default();
        cfg.vm.mem_size_mib = MiB::new(64).unwrap();
        cfg.network.subnet_mask = "/33".into();
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mem_size_mib"), "missing mem error: {msg}");
        assert!(msg.contains("CIDR"), "missing subnet error: {msg}");
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
            data_dir: data_dir.to_path_buf(),
            vm: VmConfig {
                kernel_path: data_dir.join("nonexistent-kernel"),
                ..VmConfig::default()
            },
            firecracker_bin: data_dir.join("nonexistent-firecracker"),
            ..CoopConfig::default()
        }
    }

    // Pins `mem_size_mib < 128` against `<= 128`: the boundary value
    // 128 must be accepted.
    #[test]
    fn validate_accepts_min_memory_boundary() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = validate_fixture(tmp.path());
        cfg.vm.mem_size_mib = MiB::new(128).unwrap();
        // 128 is the documented minimum; the boundary value itself must pass.
        cfg.validate().unwrap();
    }

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
        cfg.vm.kernel_path = kernel;
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
        cfg.firecracker_bin = firecracker;
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
            data_dir: PathBuf::from("/data"),
            ..CoopConfig::default()
        };
        assert_eq!(cfg.image_dir("foo"), PathBuf::from("/data/images/foo"));
        assert_eq!(
            cfg.template_path_for("foo"),
            PathBuf::from("/data/images/foo/rootfs-template.ext4")
        );
        assert_eq!(
            cfg.template_config_path_for("foo"),
            PathBuf::from("/data/images/foo/template-config.json")
        );
        assert_eq!(
            cfg.lima_base_path("foo"),
            PathBuf::from("/data/images/foo/lima-base.img")
        );
        assert_eq!(
            cfg.lima_template_path("foo"),
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
        fs::create_dir_all(cfg.image_dir("alpha")).unwrap();
        fs::create_dir_all(cfg.image_dir("beta")).unwrap();
        let images = cfg.list_images().unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].name, "alpha");
        assert_eq!(images[1].name, "beta");
    }

    #[test]
    fn instance_save_load_with_image() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("inst");
        let inst = Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0),
            dir: dir.clone(),
            image: "python-dev".to_string(),
        };
        inst.save().unwrap();
        let loaded = Instance::load(&dir).unwrap();
        assert_eq!(loaded.image, "python-dev");
    }

    #[test]
    fn instance_load_missing_image_defaults() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("inst");
        fs::create_dir_all(&dir).unwrap();
        // Write old-format instance.json without image field
        fs::write(dir.join("instance.json"), r#"{"name": "test", "index": 0}"#).unwrap();
        let loaded = Instance::load(&dir).unwrap();
        assert_eq!(loaded.image, DEFAULT_IMAGE);
    }

    // ── Resilience: corrupted instance dirs ─────────────────

    #[test]
    fn list_skips_dir_without_instance_json() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);

        make_instance(tmp.path(), "good", 0);

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

        make_instance(tmp.path(), "good", 0);

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

        make_instance(tmp.path(), "good", 0);

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
        let inst = cfg.allocate_instance(None, DEFAULT_IMAGE, None).unwrap();
        assert_eq!(inst.index, InstanceIndex::new(0));
    }

    #[test]
    fn instance_save_overwrites_atomically() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("inst");

        // Save initial state
        let inst = Instance {
            name: InstanceName::new("v1").unwrap(),
            index: InstanceIndex::new(0),
            dir: dir.clone(),
            image: DEFAULT_IMAGE.to_string(),
        };
        inst.save().unwrap();

        // Overwrite with different content
        let inst2 = Instance {
            name: InstanceName::new("v2").unwrap(),
            index: InstanceIndex::new(5),
            dir: dir.clone(),
            image: "custom".to_string(),
        };
        inst2.save().unwrap();

        // Load should see the new content, not a mix
        let loaded = Instance::load(&dir).unwrap();
        assert_eq!(loaded.name, *"v2");
        assert_eq!(loaded.index, InstanceIndex::new(5));
        assert_eq!(loaded.image, "custom");

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
        let resolved = ds.resolve(100).unwrap();
        assert_eq!(resolved.as_u32(), 150);
    }

    #[test]
    fn disk_size_resolve_relative() {
        let ds = DiskSize::Relative(GiB::new(20).unwrap());
        let resolved = ds.resolve(100).unwrap();
        assert_eq!(resolved.as_u32(), 120);
    }

    // ── Mount parsing ───────────────────────────────────────────

    #[test]
    fn mount_parse_host_only_defaults_to_workspace() {
        let tmp = TempDir::new().unwrap();
        let m = Mount::parse(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(m.host_path, tmp.path().canonicalize().unwrap());
        assert_eq!(m.guest_path, "/workspace");
    }

    #[test]
    fn mount_parse_host_and_guest() {
        let tmp = TempDir::new().unwrap();
        let spec = format!("{}:/data/project", tmp.path().display());
        let m = Mount::parse(&spec).unwrap();
        assert_eq!(m.host_path, tmp.path().canonicalize().unwrap());
        assert_eq!(m.guest_path, "/data/project");
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
            ConfigDir::Custom(PathBuf::from("/custom/path"))
        );
    }

    #[test]
    fn codex_config_dir_deserializes_custom_path() {
        let json = r#"{"codex": {"config_dir": "/custom/path"}}"#;
        let cfg: CoopConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.codex.config_dir,
            ConfigDir::Custom(PathBuf::from("/custom/path"))
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
        cfg.claude.config_dir = ConfigDir::Custom(tmp.path().to_path_buf());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_with_existing_codex_config_dir() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = CoopConfig::default();
        cfg.codex.config_dir = ConfigDir::Custom(tmp.path().to_path_buf());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_nonexistent_config_dir() {
        let mut cfg = CoopConfig::default();
        cfg.claude.config_dir = ConfigDir::Custom(PathBuf::from("/nonexistent/config"));
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("config_dir"),
            "expected config_dir error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_nonexistent_codex_config_dir() {
        let mut cfg = CoopConfig::default();
        cfg.codex.config_dir = ConfigDir::Custom(PathBuf::from("/nonexistent/config"));
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
        let inst = make_instance(tmp.path(), "foo", 0);
        let name = unique_instance_name("foo", &[inst]).unwrap();
        assert_eq!(name, *"foo-2");
    }

    #[test]
    fn unique_name_multiple_collisions() {
        let tmp = TempDir::new().unwrap();
        let i1 = make_instance(tmp.path(), "foo", 0);
        let i2 = make_instance(tmp.path(), "foo-2", 1);
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
            .allocate_instance(None, DEFAULT_IMAGE, Some(&ws))
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
            .allocate_instance(None, DEFAULT_IMAGE, Some(&ws))
            .unwrap();
        assert_eq!(inst1.name, *"dupe");

        // Second allocation with same basename gets -2 suffix
        let inst2 = cfg
            .allocate_instance(None, DEFAULT_IMAGE, Some(&ws))
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
            .allocate_instance(Some("custom"), DEFAULT_IMAGE, Some(&ws))
            .unwrap();
        assert_eq!(inst.name, *"custom");
    }

    #[test]
    fn allocate_without_name_or_workspace_uses_index() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp);
        let inst = cfg.allocate_instance(None, DEFAULT_IMAGE, None).unwrap();
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
}
