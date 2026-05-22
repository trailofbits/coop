//! `devcontainer.json` translator.
//!
//! Reads a subset of the [devcontainer.json](https://containers.dev/) spec
//! and maps recognised properties to coop's existing primitives. The full
//! design (UX, scope, precedence, edge cases) is captured in issue #129;
//! the docstring on each public item summarises the v1 contract.
//!
//! Coop reads a **subset** of devcontainer.json, not the full spec.
//! Unsupported properties produce a `warn-and-skip` entry in the report
//! rather than silently degrading.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{self, CoopConfig, GiB, MiB, Mount, PortForward};
use crate::guest::{BuiltinProfile, builtin_for_feature};
use crate::guest_env_state::EnvVarName;

/// Default relative path coop looks for inside a workspace or mount root.
pub const DEFAULT_DEVCONTAINER_REL_PATH: &str = ".devcontainer/devcontainer.json";

// ── Parsing ──────────────────────────────────────────────────

/// Convert JSONC (`//`, `/* */`, trailing commas) to plain JSON.
///
/// Implements a single-pass byte scanner aware of string literals (with
/// `\"` and `\\` escapes). Trailing commas are detected by buffering each
/// `,` and dropping it if the next non-whitespace, non-comment token is
/// `]` or `}`. Whitespace bytes are preserved so error offsets in
/// downstream `serde_json` diagnostics roughly track the source.
#[must_use]
pub fn jsonc_to_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut pending_comma = false;
    let mut in_string = false;

    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            if pending_comma {
                out.push(',');
                pending_comma = false;
            }
            out.push(c as char);
            if c == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                if pending_comma {
                    out.push(',');
                    pending_comma = false;
                }
                in_string = true;
                out.push('"');
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    i += 2;
                }
            }
            b',' => {
                pending_comma = true;
                i += 1;
            }
            b']' | b'}' => {
                pending_comma = false;
                out.push(c as char);
                i += 1;
            }
            b if b.is_ascii_whitespace() => {
                out.push(b as char);
                i += 1;
            }
            _ => {
                if pending_comma {
                    out.push(',');
                    pending_comma = false;
                }
                out.push(c as char);
                i += 1;
            }
        }
    }
    if pending_comma {
        // Trailing comma at EOF without a closer; let serde report it.
        out.push(',');
    }
    out
}

/// Raw deserialization shape — close to the spec.
///
/// Field names are camelCase per devcontainer.json. `serde_json::Value`
/// captures fields with flexible shapes (e.g. `forwardPorts` items can be
/// numbers or strings) so we can render specific error messages instead of
/// failing the whole parse on one weird entry.
#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
struct RawDevcontainer {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    image: Option<serde_json::Value>,
    #[serde(default)]
    build: Option<serde_json::Value>,
    #[serde(default)]
    docker_compose_file: Option<serde_json::Value>,
    #[serde(default)]
    features: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    forward_ports: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    container_env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    remote_user: Option<String>,
    #[serde(default)]
    post_start_command: Option<serde_json::Value>,
    #[serde(default)]
    mounts: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    host_requirements: Option<RawHostRequirements>,
    #[serde(default)]
    customizations: Option<serde_json::Value>,
    /// Anything we don't model is captured here so we can emit a single
    /// `unrecognised key` row in the report rather than silently dropping it.
    #[serde(flatten)]
    extras: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
struct RawHostRequirements {
    #[serde(default)]
    cpus: Option<u32>,
    #[serde(default)]
    memory: Option<MemorySpec>,
    #[serde(default)]
    storage: Option<MemorySpec>,
    #[serde(flatten)]
    extras: serde_json::Map<String, serde_json::Value>,
}

/// A parsed devcontainer.json file. Cheap to clone; field semantics match
/// the spec verbatim.
#[derive(Debug)]
pub struct ParsedDevcontainer {
    pub path: PathBuf,
    raw: RawDevcontainer,
}

impl ParsedDevcontainer {
    /// Parse JSONC text from `path`. The path is retained for reporting.
    pub fn from_str(path: PathBuf, text: &str) -> Result<Self> {
        let json = jsonc_to_json(text);
        let raw: RawDevcontainer = serde_json::from_str(&json)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(Self { path, raw })
    }

    /// Read and parse a file from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        Self::from_str(path.to_path_buf(), &text)
    }
}

// ── Reporting ────────────────────────────────────────────────

/// Result of translating one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportStatus {
    /// Value from devcontainer.json took effect.
    Applied,
    /// Devcontainer value was overridden by a CLI flag or existing config.
    Overridden,
    /// Property is not supported by coop; ignored with a warning.
    Unsupported,
    /// Property value could not be parsed; ignored with a warning.
    Invalid,
}

impl ReportStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Overridden => "overridden",
            Self::Unsupported => "unsupported",
            Self::Invalid => "invalid",
        }
    }
}

/// Origin of the effective value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportSource {
    Cli,
    Devcontainer,
}

impl ReportSource {
    fn label(self) -> &'static str {
        match self {
            Self::Cli => "CLI",
            Self::Devcontainer => "devcontainer.json",
        }
    }
}

/// One row in the report table. `key` is the dotted devcontainer.json path
/// (e.g. `hostRequirements.cpus`); `value` is the effective value (after
/// CLI/precedence) rendered as a short string; `note` is free-form context.
#[derive(Debug, Clone)]
pub struct ReportEntry {
    pub key: String,
    pub status: ReportStatus,
    pub source: ReportSource,
    pub value: String,
    pub note: String,
}

/// Loud per-key report rendered after translation. Implements `Display`
/// for direct `writeln!` into stderr.
#[derive(Debug, Default)]
pub struct Report {
    pub entries: Vec<ReportEntry>,
    pub source_path: Option<PathBuf>,
    /// Discovered but not chosen devcontainer.json files (workspace wins).
    pub ignored_paths: Vec<PathBuf>,
}

impl Report {
    fn push(
        &mut self,
        key: impl Into<String>,
        status: ReportStatus,
        source: ReportSource,
        value: impl Into<String>,
        note: impl Into<String>,
    ) {
        self.entries.push(ReportEntry {
            key: key.into(),
            status,
            source,
            value: value.into(),
            note: note.into(),
        });
    }

    /// Format the report as a plain-text table suitable for stderr.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(p) = &self.source_path {
            let _ = writeln!(out, "devcontainer.json: {}", p.display());
        }
        for ignored in &self.ignored_paths {
            let _ = writeln!(
                out,
                "  ignored: {} (workspace's takes precedence)",
                ignored.display()
            );
        }
        if self.entries.is_empty() {
            let _ = writeln!(out, "  (no recognised keys)");
            return out;
        }
        let headers = ["Key", "Status", "Source", "Value", "Note"];
        let mut widths = headers.map(str::len);
        for e in &self.entries {
            widths[0] = widths[0].max(e.key.len());
            widths[1] = widths[1].max(e.status.label().len());
            widths[2] = widths[2].max(e.source.label().len());
            widths[3] = widths[3].max(e.value.len());
            widths[4] = widths[4].max(e.note.len());
        }
        let row = |out: &mut String, cells: [&str; 5]| {
            for (i, c) in cells.iter().enumerate() {
                if i > 0 {
                    out.push_str("  ");
                }
                let _ = write!(out, "{c:<w$}", w = widths[i]);
            }
            out.push('\n');
        };
        row(&mut out, headers);
        let dashes: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        row(
            &mut out,
            [
                dashes[0].as_str(),
                dashes[1].as_str(),
                dashes[2].as_str(),
                dashes[3].as_str(),
                dashes[4].as_str(),
            ],
        );
        for e in &self.entries {
            row(
                &mut out,
                [
                    e.key.as_str(),
                    e.status.label(),
                    e.source.label(),
                    e.value.as_str(),
                    e.note.as_str(),
                ],
            );
        }
        out
    }
}

// ── Inputs and outputs of the translator ─────────────────────

/// Subset of CLI/config state the translator needs to honour the
/// "CLI > devcontainer.json > defaults" precedence and report overrides.
///
/// All fields share the `cli_` prefix on purpose: the report needs to
/// distinguish CLI-sourced values from any other state, and the prefix
/// reads as a stable boundary in call sites that build this struct.
#[expect(clippy::struct_field_names, reason = "cli_ prefix is load-bearing")]
#[derive(Debug, Default)]
pub struct TranslatorInputs {
    pub cli_vcpus: Option<u8>,
    pub cli_mem_mib: Option<MiB>,
    pub cli_disk_gib: Option<GiB>,
    pub cli_post_start: Option<String>,
    pub cli_guest_env_keys: Vec<EnvVarName>,
    pub cli_forward_ports: Vec<PortForward>,
    pub cli_mounts: Vec<Mount>,
    pub cli_profiles: Vec<String>,
    /// True when the caller passed `--workspace` or `--git-repo`. coop's
    /// `start` path uses `--mount` only when neither of these is set, so
    /// devcontainer `mounts` are dropped with a report row pointing at
    /// the conflict rather than silently disappearing.
    pub cli_workspace_or_git_repo: bool,
}

/// Stage of the lifecycle the translator is feeding into.
///
/// `Start` and `Setup` care about disjoint subsets of devcontainer.json
/// keys. Modelling this as an enum keeps "applied for this command" vs
/// "applied for a different command" distinct from "unsupported".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Setup,
    Start,
}

/// Result of `translate` — the values to push into config plus a report.
#[derive(Debug, Default)]
pub struct Translation {
    pub vcpus: Option<u8>,
    pub mem_mib: Option<MiB>,
    pub disk_gib: Option<GiB>,
    pub post_start: Option<String>,
    pub guest_env: BTreeMap<EnvVarName, String>,
    pub forward_ports: Vec<PortForward>,
    pub mounts: Vec<Mount>,
    pub profiles: Vec<String>,
    pub report: Report,
}

// ── Translation ──────────────────────────────────────────────

/// Apply the parsed devcontainer.json against CLI/config inputs.
///
/// Returns effective overrides plus a human-readable report. The
/// translator never mutates the input; callers fold the returned values
/// into `CoopConfig`/`StartOpts`/`SetupOptions`.
#[expect(
    clippy::too_many_lines,
    reason = "single readable dispatch — splitting into per-key helpers fragments per-key context"
)]
pub fn translate(
    file: &ParsedDevcontainer,
    inputs: &TranslatorInputs,
    stage: Stage,
) -> Translation {
    let mut t = Translation::default();
    t.report.source_path = Some(file.path.clone());

    if let Some(reqs) = &file.raw.host_requirements {
        translate_host_requirements(reqs, inputs, &mut t);
    }

    if let Some(cmd) = &file.raw.post_start_command {
        let key = "postStartCommand";
        if stage == Stage::Setup {
            t.report.push(
                key,
                ReportStatus::Applied,
                ReportSource::Devcontainer,
                render_value(cmd),
                "applied at start time",
            );
        } else if let Some(s) = post_start_to_string(cmd) {
            if let Some(cli) = &inputs.cli_post_start {
                t.report.push(
                    key,
                    ReportStatus::Overridden,
                    ReportSource::Cli,
                    cli.clone(),
                    format!("CLI --post-start overrides devcontainer value {s:?}"),
                );
            } else {
                t.post_start = Some(s.clone());
                t.report.push(
                    key,
                    ReportStatus::Applied,
                    ReportSource::Devcontainer,
                    s,
                    "",
                );
            }
        } else {
            t.report.push(
                key,
                ReportStatus::Invalid,
                ReportSource::Devcontainer,
                render_value(cmd),
                "expected string or [string,...]; array form joined with ' && '",
            );
        }
    }

    if let Some(env) = &file.raw.container_env {
        translate_container_env(env, inputs, stage, &mut t);
    }

    if let Some(ports) = &file.raw.forward_ports {
        translate_forward_ports(ports, inputs, stage, &mut t);
    }

    if let Some(features) = &file.raw.features {
        translate_features(features, inputs, stage, &mut t);
    }

    if let Some(mounts) = &file.raw.mounts {
        translate_mounts(mounts, inputs, stage, &mut t);
    }

    if let Some(user) = &file.raw.remote_user {
        let key = "remoteUser";
        if user == "ubuntu" {
            t.report.push(
                key,
                ReportStatus::Applied,
                ReportSource::Devcontainer,
                user.clone(),
                "matches coop's hardcoded guest user",
            );
        } else {
            t.report.push(
                key,
                ReportStatus::Unsupported,
                ReportSource::Devcontainer,
                user.clone(),
                "coop hardcodes the guest user to 'ubuntu' in v1",
            );
        }
    }

    if file.raw.image.is_some() {
        t.report.push(
            "image",
            ReportStatus::Unsupported,
            ReportSource::Devcontainer,
            render_value_opt(file.raw.image.as_ref()),
            "coop builds its own rootfs; base images are ignored",
        );
    }
    if file.raw.build.is_some() {
        t.report.push(
            "build",
            ReportStatus::Unsupported,
            ReportSource::Devcontainer,
            render_value_opt(file.raw.build.as_ref()),
            "coop builds its own rootfs; Dockerfile/build is ignored",
        );
    }
    if file.raw.docker_compose_file.is_some() {
        t.report.push(
            "dockerComposeFile",
            ReportStatus::Unsupported,
            ReportSource::Devcontainer,
            render_value_opt(file.raw.docker_compose_file.as_ref()),
            "coop has no compose support",
        );
    }
    if file.raw.customizations.is_some() {
        t.report.push(
            "customizations",
            ReportStatus::Unsupported,
            ReportSource::Devcontainer,
            "<...>",
            "editor-specific; not relevant to coop",
        );
    }

    if let Some(name) = &file.raw.name {
        t.report.push(
            "name",
            ReportStatus::Unsupported,
            ReportSource::Devcontainer,
            name.clone(),
            "informational only; coop names instances via --name or auto-generates",
        );
    }

    for (key, value) in &file.raw.extras {
        t.report.push(
            key,
            ReportStatus::Unsupported,
            ReportSource::Devcontainer,
            render_value(value),
            "unrecognised devcontainer.json key",
        );
    }

    t
}

fn translate_host_requirements(
    reqs: &RawHostRequirements,
    inputs: &TranslatorInputs,
    t: &mut Translation,
) {
    if let Some(cpus) = reqs.cpus {
        let key = "hostRequirements.cpus";
        match u8::try_from(cpus) {
            Ok(n) if n > 0 => {
                if let Some(cli) = inputs.cli_vcpus {
                    t.report.push(
                        key,
                        ReportStatus::Overridden,
                        ReportSource::Cli,
                        cli.to_string(),
                        format!("CLI --vcpus overrides devcontainer.json value {n}"),
                    );
                } else {
                    t.vcpus = Some(n);
                    t.report.push(
                        key,
                        ReportStatus::Applied,
                        ReportSource::Devcontainer,
                        n.to_string(),
                        "",
                    );
                }
            }
            _ => t.report.push(
                key,
                ReportStatus::Invalid,
                ReportSource::Devcontainer,
                cpus.to_string(),
                "expected positive integer that fits in u8",
            ),
        }
    }
    if let Some(mem) = reqs.memory {
        let key = "hostRequirements.memory";
        let mib = mem.as_mib();
        if let Some(cli) = inputs.cli_mem_mib {
            t.report.push(
                key,
                ReportStatus::Overridden,
                ReportSource::Cli,
                format!("{cli} MiB"),
                format!("CLI --mem overrides devcontainer.json value {mib} MiB"),
            );
        } else {
            t.mem_mib = Some(mib);
            t.report.push(
                key,
                ReportStatus::Applied,
                ReportSource::Devcontainer,
                format!("{mib} MiB"),
                "",
            );
        }
    }
    if let Some(storage) = reqs.storage {
        let key = "hostRequirements.storage";
        let gib = storage.as_gib();
        if let Some(cli) = inputs.cli_disk_gib {
            t.report.push(
                key,
                ReportStatus::Overridden,
                ReportSource::Cli,
                format!("{cli} GiB"),
                format!("CLI --disk overrides devcontainer.json value {gib} GiB"),
            );
        } else {
            t.disk_gib = Some(gib);
            t.report.push(
                key,
                ReportStatus::Applied,
                ReportSource::Devcontainer,
                format!("{gib} GiB"),
                "",
            );
        }
    }
    for (k, v) in &reqs.extras {
        t.report.push(
            format!("hostRequirements.{k}"),
            ReportStatus::Unsupported,
            ReportSource::Devcontainer,
            render_value(v),
            "unrecognised hostRequirements key",
        );
    }
}

fn translate_container_env(
    env: &BTreeMap<String, String>,
    inputs: &TranslatorInputs,
    stage: Stage,
    t: &mut Translation,
) {
    let key = "containerEnv";
    if env.is_empty() {
        return;
    }
    if stage == Stage::Setup {
        t.report.push(
            key,
            ReportStatus::Applied,
            ReportSource::Devcontainer,
            format!("{} entries", env.len()),
            "applied at start time",
        );
        return;
    }
    let cli_keys: std::collections::HashSet<&str> = inputs
        .cli_guest_env_keys
        .iter()
        .map(EnvVarName::as_str)
        .collect();
    let mut overridden = Vec::new();
    let mut applied = Vec::new();
    let mut invalid = Vec::new();
    for (k, v) in env {
        let Ok(name) = EnvVarName::new(k) else {
            invalid.push(k.clone());
            continue;
        };
        if cli_keys.contains(k.as_str()) {
            overridden.push(k.clone());
        } else {
            t.guest_env.insert(name, v.clone());
            applied.push(k.clone());
        }
    }
    if !applied.is_empty() {
        t.report.push(
            key,
            ReportStatus::Applied,
            ReportSource::Devcontainer,
            format!("{} entries", applied.len()),
            applied.join(","),
        );
    }
    if !overridden.is_empty() {
        t.report.push(
            key,
            ReportStatus::Overridden,
            ReportSource::Cli,
            format!("{} entries", overridden.len()),
            format!("CLI --env overrides: {}", overridden.join(",")),
        );
    }
    if !invalid.is_empty() {
        t.report.push(
            key,
            ReportStatus::Invalid,
            ReportSource::Devcontainer,
            format!("{} entries", invalid.len()),
            format!(
                "invalid env var names (must match [a-zA-Z_][a-zA-Z0-9_]*): {}",
                invalid.join(","),
            ),
        );
    }
}

fn translate_forward_ports(
    ports: &[serde_json::Value],
    inputs: &TranslatorInputs,
    stage: Stage,
    t: &mut Translation,
) {
    let key = "forwardPorts";
    if ports.is_empty() {
        return;
    }
    if stage == Stage::Setup {
        t.report.push(
            key,
            ReportStatus::Applied,
            ReportSource::Devcontainer,
            format!("{} entries", ports.len()),
            "applied at start time",
        );
        return;
    }
    let cli_guest_ports: std::collections::HashSet<u16> = inputs
        .cli_forward_ports
        .iter()
        .map(|p| p.guest.get())
        .collect();
    let mut applied: Vec<String> = Vec::new();
    let mut overridden: Vec<String> = Vec::new();
    for entry in ports {
        let parsed = parse_forward_port_entry(entry);
        match parsed {
            Ok(pf) => {
                if cli_guest_ports.contains(&pf.guest.get()) {
                    overridden.push(pf.guest.to_string());
                } else {
                    applied.push(format_pf(&pf));
                    t.forward_ports.push(pf);
                }
            }
            Err(e) => t.report.push(
                key,
                ReportStatus::Invalid,
                ReportSource::Devcontainer,
                render_value(entry),
                e.to_string(),
            ),
        }
    }
    if !applied.is_empty() {
        t.report.push(
            key,
            ReportStatus::Applied,
            ReportSource::Devcontainer,
            applied.join(","),
            "",
        );
    }
    if !overridden.is_empty() {
        t.report.push(
            key,
            ReportStatus::Overridden,
            ReportSource::Cli,
            overridden.join(","),
            format!(
                "CLI --forward-port overrides for guest port(s): {}",
                overridden.join(",")
            ),
        );
    }
}

fn translate_features(
    features: &BTreeMap<String, serde_json::Value>,
    inputs: &TranslatorInputs,
    stage: Stage,
    t: &mut Translation,
) {
    if features.is_empty() {
        return;
    }
    let cli_profiles: std::collections::HashSet<&str> =
        inputs.cli_profiles.iter().map(String::as_str).collect();
    for raw_id in features.keys() {
        let key = format!("features.{raw_id}");
        let Some(builtin) = map_feature_to_profile(raw_id) else {
            t.report.push(
                key,
                ReportStatus::Unsupported,
                ReportSource::Devcontainer,
                raw_id.clone(),
                "no built-in profile match; coop does not pull OCI feature images in v1",
            );
            continue;
        };
        let name = builtin.name;
        if stage == Stage::Start {
            t.report.push(
                key,
                ReportStatus::Unsupported,
                ReportSource::Devcontainer,
                name.to_string(),
                "features are baked into the template at `coop setup` time, \
                 not selected per-start",
            );
            continue;
        }
        if cli_profiles.contains(&name) {
            t.report.push(
                key,
                ReportStatus::Overridden,
                ReportSource::Cli,
                name.to_string(),
                "CLI --profile already includes this profile",
            );
            continue;
        }
        t.profiles.push(name.to_string());
        t.report.push(
            key,
            ReportStatus::Applied,
            ReportSource::Devcontainer,
            name.to_string(),
            format!("mapped to built-in profile '{name}'"),
        );
    }
}

fn translate_mounts(
    mounts: &[serde_json::Value],
    inputs: &TranslatorInputs,
    stage: Stage,
    t: &mut Translation,
) {
    let key = "mounts";
    if mounts.is_empty() {
        return;
    }
    if stage == Stage::Setup {
        t.report.push(
            key,
            ReportStatus::Applied,
            ReportSource::Devcontainer,
            format!("{} entries", mounts.len()),
            "applied at start time",
        );
        return;
    }
    let cli_present = !inputs.cli_mounts.is_empty();
    if cli_present {
        t.report.push(
            key,
            ReportStatus::Overridden,
            ReportSource::Cli,
            format!("{} CLI --mount entries", inputs.cli_mounts.len()),
            "CLI --mount conflicts with --workspace and replaces devcontainer mounts",
        );
        return;
    }
    if inputs.cli_workspace_or_git_repo {
        t.report.push(
            key,
            ReportStatus::Unsupported,
            ReportSource::Devcontainer,
            format!("{} entries", mounts.len()),
            "coop --mount is mutually exclusive with --workspace/--git-repo in v1; \
             drop those flags to use devcontainer mounts",
        );
        return;
    }
    let mut applied: Vec<String> = Vec::new();
    for entry in mounts {
        match parse_mount_entry(entry) {
            Ok(m) => {
                applied.push(format!("{}->{}", m.host_path.display(), m.guest_path));
                t.mounts.push(m);
            }
            Err(e) => t.report.push(
                key,
                ReportStatus::Invalid,
                ReportSource::Devcontainer,
                render_value(entry),
                e.to_string(),
            ),
        }
    }
    if !applied.is_empty() {
        t.report.push(
            key,
            ReportStatus::Applied,
            ReportSource::Devcontainer,
            applied.join(", "),
            "",
        );
    }
}

// ── Helpers ──────────────────────────────────────────────────

fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

fn render_value_opt(v: Option<&serde_json::Value>) -> String {
    v.map(render_value).unwrap_or_default()
}

fn post_start_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let strs: Option<Vec<&str>> = items.iter().map(serde_json::Value::as_str).collect();
            strs.map(|ss| ss.join(" && "))
        }
        _ => None,
    }
}

/// Memory or storage size parsed from devcontainer.json `hostRequirements`.
///
/// Stored as `NonZeroU32` mebibytes (rounded up from the parsed byte
/// count). Parsing rejects zero, non-finite values, and overflow at the
/// deserialize boundary so downstream code can treat the value as a
/// validated quantity.
///
/// Accepts the [devcontainer spec][spec] decimal units (`KB`, `MB`, `GB`,
/// `TB`), single-letter aliases for those (`K`, `M`, `G`, `T`), binary
/// suffixes (`KiB`, `MiB`, `GiB`, `TiB`) that real-world files use
/// interchangeably, a bare `B` suffix, and bare unit-less integers
/// (interpreted as bytes per the Kubernetes convention). Matching is
/// case-insensitive.
///
/// [spec]: https://containers.dev/implementors/json_reference/#image-specific
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySpec(NonZeroU32);

impl MemorySpec {
    /// Parse a size string per the formats described in the type-level doc.
    pub fn parse(s: &str) -> Result<Self> {
        let bytes = parse_size_bytes(s)?;
        let mib = bytes.div_ceil(1024 * 1024);
        let mib_u32 =
            u32::try_from(mib).with_context(|| format!("size '{s}' overflows u32 MiB"))?;
        let nz = NonZeroU32::new(mib_u32)
            .with_context(|| format!("size '{s}' must be greater than zero"))?;
        Ok(Self(nz))
    }

    /// Mebibytes (rounded up).
    pub fn as_mib(self) -> MiB {
        // `MiB` and `MemorySpec` share the same `NonZeroU32` invariant,
        // so the wrap is infallible.
        MiB::from_nonzero(self.0)
    }

    /// Gibibytes (rounded up from the stored MiB).
    pub fn as_gib(self) -> GiB {
        #[expect(
            clippy::expect_used,
            reason = "div_ceil of a non-zero by 1024 stays non-zero"
        )]
        let nz = NonZeroU32::new(self.0.get().div_ceil(1024))
            .expect("div_ceil of non-zero MiB by 1024 is non-zero");
        GiB::from_nonzero(nz)
    }
}

impl fmt::Display for MemorySpec {
    #[mutants::skip] // equivalent: callers don't assert the formatted output, only the numeric accessors
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} MiB", self.0)
    }
}

impl<'de> Deserialize<'de> for MemorySpec {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(|e| serde::de::Error::custom(format!("{e:#}")))
    }
}

fn parse_size_bytes(s: &str) -> Result<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        bail!("size string is empty");
    }
    // Bare unit-less integer — interpret as bytes (Kubernetes convention).
    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return trimmed
            .parse::<u64>()
            .with_context(|| format!("size '{s}' overflows u64 bytes"));
    }
    let (num_str, mult) = split_size(trimmed)?;
    let num: f64 = num_str.parse().with_context(|| {
        format!("expected '<decimal><unit>' (e.g. '4GB') or bare bytes; got '{s}'")
    })?;
    if !num.is_finite() || num < 0.0 {
        bail!("size must be a non-negative finite number: '{s}'");
    }
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "sizes are bounded well below f64 precision and u64 max"
    )]
    let bytes = (num * mult as f64).round() as u64;
    Ok(bytes)
}

/// Split a `<decimal><unit>` string into the numeric part and a byte
/// multiplier. Suffix matching is case-insensitive and accepts both
/// decimal (`GB`) and binary (`GiB`) units. Single-letter shorthands
/// (`K`/`M`/`G`/`T`) are decimal aliases (e.g. `1g` == `1GB`).
fn split_size(s: &str) -> Result<(&str, u64)> {
    // Longest suffix first so "GiB" matches before "GB" matches before "G".
    let suffixes: &[(&str, u64)] = &[
        ("TiB", 1u64 << 40),
        ("GiB", 1u64 << 30),
        ("MiB", 1u64 << 20),
        ("KiB", 1u64 << 10),
        ("TB", 1_000_000_000_000),
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("KB", 1_000),
        ("T", 1_000_000_000_000),
        ("G", 1_000_000_000),
        ("M", 1_000_000),
        ("K", 1_000),
        ("B", 1),
    ];
    let lower = s.to_ascii_lowercase();
    for (suf, mult) in suffixes {
        let lower_suf = suf.to_ascii_lowercase();
        if lower.ends_with(&lower_suf) {
            let cut = s.len() - suf.len();
            return Ok((s[..cut].trim(), *mult));
        }
    }
    bail!("expected a unit suffix (B, KB, MB, GB, TB; KiB/MiB/GiB/TiB also accepted): '{s}'");
}

fn parse_forward_port_entry(v: &serde_json::Value) -> Result<PortForward> {
    match v {
        serde_json::Value::Number(n) => {
            let n = n.as_u64().context("port must be a positive integer")?;
            let n: u16 = n
                .try_into()
                .with_context(|| format!("port {n} out of range 1..=65535"))?;
            PortForward::parse(&n.to_string())
        }
        serde_json::Value::String(s) => {
            // devcontainer.json supports "host:port" string forms; coop
            // does not host-IP-bind. We accept "8080" or "8080:9090".
            PortForward::parse(s)
        }
        _ => bail!("expected port number or 'GUEST[:HOST]' string"),
    }
}

fn parse_mount_entry(v: &serde_json::Value) -> Result<Mount> {
    match v {
        serde_json::Value::String(s) => parse_mount_string(s),
        serde_json::Value::Object(map) => {
            let typ = map
                .get("type")
                .and_then(|v| v.as_str())
                .context("mount object requires 'type'")?;
            if typ != "bind" {
                bail!("unsupported mount type '{typ}' (coop supports 'bind' only in v1)");
            }
            let source = map
                .get("source")
                .and_then(|v| v.as_str())
                .context("mount object requires 'source'")?;
            let target = map
                .get("target")
                .and_then(|v| v.as_str())
                .context("mount object requires 'target'")?;
            Mount::from_parts(source, crate::paths::GuestPath::absolute(target)?)
        }
        _ => bail!("expected mount string or object"),
    }
}

/// Parse a devcontainer mount string into a [`Mount`].
///
/// Accepts both coop's native `HOST_PATH[:GUEST_PATH]` form (deferred to
/// [`Mount::parse`]) and Docker's `type=bind,source=...,target=...` form
/// (parsed here and constructed via [`Mount::from_parts`]). Either way
/// the validated `Mount` value is the single output — no intermediate
/// string round-trip.
fn parse_mount_string(s: &str) -> Result<Mount> {
    if !s.contains('=') {
        // Native HOST:GUEST or HOST form — let `Mount::parse` handle it.
        return Mount::parse(s);
    }
    let mut typ: Option<&str> = None;
    let mut source: Option<&str> = None;
    let mut target: Option<&str> = None;
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.trim())),
            // Bare token: only meaningful for flag-style fields like `readonly`.
            None => (part, None),
        };
        match k {
            "type" => typ = v,
            "source" | "src" => source = v,
            "target" | "dst" | "destination" => target = v,
            "readonly" | "ro" | "consistency" | "bind-propagation" => {
                // Tolerate Docker flags we don't honour in v1; the bind
                // semantics are coop's default.
            }
            other => bail!("unsupported mount field '{other}'"),
        }
    }
    let typ = typ.unwrap_or("bind");
    if typ != "bind" {
        bail!("unsupported mount type '{typ}' (coop supports 'bind' only in v1)");
    }
    let source = source.context("mount string missing source=...")?;
    let target = target.context("mount string missing target=...")?;
    Mount::from_parts(source, crate::paths::GuestPath::absolute(target)?)
}

/// Map a devcontainer feature id (raw key) to a built-in coop profile,
/// or `None` if no match. We recognise both bare names (`rust`) and the
/// fully-qualified registry form (`ghcr.io/devcontainers/features/rust:1`).
fn map_feature_to_profile(raw: &str) -> Option<&'static BuiltinProfile> {
    // Trim `ghcr.io/devcontainers/features/<name>[:version]` to `<name>`.
    let id = raw
        .rsplit('/')
        .next()
        .unwrap_or(raw)
        .split(':')
        .next()
        .unwrap_or(raw);
    builtin_for_feature(id)
}

fn format_pf(p: &PortForward) -> String {
    if p.guest == p.host {
        p.guest.to_string()
    } else {
        format!("{}:{}", p.guest, p.host)
    }
}

// ── Discovery ────────────────────────────────────────────────

/// A devcontainer.json file found on disk, with its origin recorded so
/// the report can distinguish "from workspace" vs "from mount".
#[derive(Debug, Clone)]
pub struct Discovered {
    pub path: PathBuf,
    pub origin: DiscoveryOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryOrigin {
    Workspace,
    Mount,
}

/// Look for `.devcontainer/devcontainer.json` under each candidate root.
///
/// Returns matches in order: workspace first (when present), then each
/// mount root in the order supplied. Non-existent files are silently
/// skipped — only present files are returned.
pub fn discover(workspace: Option<&Path>, mounts: &[Mount]) -> Vec<Discovered> {
    let mut out = Vec::new();
    if let Some(ws) = workspace {
        let candidate = ws.join(DEFAULT_DEVCONTAINER_REL_PATH);
        if candidate.is_file() {
            out.push(Discovered {
                path: candidate,
                origin: DiscoveryOrigin::Workspace,
            });
        }
    }
    for m in mounts {
        let candidate = m.host_path.join(DEFAULT_DEVCONTAINER_REL_PATH);
        if candidate.is_file() {
            out.push(Discovered {
                path: candidate,
                origin: DiscoveryOrigin::Mount,
            });
        }
    }
    out
}

/// Pick the winning devcontainer.json and record losers in the report.
///
/// Workspace > mounts. Ties within mounts are broken by order (first
/// `--mount` wins). The ignored set is populated even when the caller
/// doesn't end up loading the file, so `--dry-run` shows the user every
/// file we saw.
#[must_use]
pub fn pick_winner(found: Vec<Discovered>) -> Option<(Discovered, Vec<PathBuf>)> {
    let mut workspace = None;
    let mut mounts: Vec<Discovered> = Vec::new();
    for d in found {
        match d.origin {
            DiscoveryOrigin::Workspace => workspace = Some(d),
            DiscoveryOrigin::Mount => mounts.push(d),
        }
    }
    if let Some(ws) = workspace {
        let losers = mounts.into_iter().map(|d| d.path).collect();
        return Some((ws, losers));
    }
    if let Some(first) = mounts.first().cloned() {
        let losers = mounts.into_iter().skip(1).map(|d| d.path).collect();
        return Some((first, losers));
    }
    None
}

// ── Application ──────────────────────────────────────────────

/// Apply a translation to `CoopConfig`. Returns warnings to emit.
///
/// Returns a vector of human-readable warnings (e.g. "containerEnv X
/// overrides forwarded value") so callers can decide whether to emit at
/// WARN or DEBUG. `guest_env` precedence already wins over forwarded
/// values per the existing `prepare_env_forwarding` semantics.
pub fn apply_to_config(cfg: &mut CoopConfig, t: &Translation) -> Result<()> {
    if let Some(v) = t.vcpus {
        cfg.vm.vcpu_count =
            std::num::NonZeroU8::new(v).context("translated --vcpus value resolved to zero")?;
    }
    if let Some(m) = t.mem_mib {
        cfg.vm.mem_size_mib = m;
    }
    if let Some(p) = &t.post_start {
        cfg.post_start = Some(p.clone());
    }
    for (k, v) in &t.guest_env {
        cfg.guest_env.insert(k.clone(), v.clone());
    }
    Ok(())
}

/// Return the effective disk override that a caller should pass through
/// to `StartOpts::disk`. CLI value wins; otherwise the translator's
/// `disk_gib` is used.
#[must_use]
pub fn effective_disk(cli: Option<GiB>, t: &Translation) -> Option<GiB> {
    cli.or(t.disk_gib)
}

/// Merge devcontainer.json forwards on top of existing config defaults.
///
/// Mirrors `config::merge_forward_ports` semantics — later entries with
/// the same guest port win. The caller still merges the result with the
/// CLI forwards via the existing helper.
#[must_use]
pub fn merge_into_forward_ports(
    cfg_forwards: &[PortForward],
    t_forwards: &[PortForward],
) -> Vec<PortForward> {
    config::merge_forward_ports(cfg_forwards, t_forwards)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests use unwrap for brevity")]
mod tests {
    use super::*;

    fn parse(text: &str) -> ParsedDevcontainer {
        ParsedDevcontainer::from_str(PathBuf::from("test.json"), text).unwrap()
    }

    #[test]
    fn jsonc_strips_line_comments() {
        let src = r#"{
            // a comment
            "name": "demo" // trailing
        }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["name"], "demo");
    }

    #[test]
    fn jsonc_strips_block_comments() {
        let src = r#"{ /* block */ "k": /* inline */ 1 }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["k"], 1);
    }

    #[test]
    fn jsonc_strips_trailing_commas() {
        let src = r#"{ "a": 1, "b": [1, 2, 3,], }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"][2], 3);
    }

    #[test]
    fn jsonc_keeps_commas_inside_strings() {
        let src = r#"{ "k": "a, b, c", }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["k"], "a, b, c");
    }

    #[test]
    fn jsonc_keeps_slashes_inside_strings() {
        let src = r#"{ "k": "https://example.com/a", "j": "/* not a comment */" }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["k"], "https://example.com/a");
        assert_eq!(v["j"], "/* not a comment */");
    }

    #[test]
    fn parse_supported_subset() {
        let f = parse(
            r#"{
                "name": "demo",
                "image": "ignored",
                "hostRequirements": { "cpus": 4, "memory": "4GiB", "storage": "16GiB" },
                "containerEnv": { "FOO": "bar" },
                "forwardPorts": [3000, "8080:8081"],
                "postStartCommand": "echo hi",
                "features": { "ghcr.io/devcontainers/features/rust:1": {} },
                "remoteUser": "root"
            }"#,
        );
        assert_eq!(f.raw.name.as_deref(), Some("demo"));
        assert_eq!(f.raw.host_requirements.as_ref().unwrap().cpus, Some(4));
    }

    #[test]
    fn translate_marks_overridden_when_cli_wins() {
        let f = parse(r#"{ "hostRequirements": { "cpus": 4 }, "postStartCommand": "x" }"#);
        let inputs = TranslatorInputs {
            cli_vcpus: Some(8),
            cli_post_start: Some("y".to_string()),
            ..TranslatorInputs::default()
        };
        let t = translate(&f, &inputs, Stage::Start);
        let cpu = t
            .report
            .entries
            .iter()
            .find(|e| e.key == "hostRequirements.cpus")
            .unwrap();
        assert_eq!(cpu.status, ReportStatus::Overridden);
        let cmd = t
            .report
            .entries
            .iter()
            .find(|e| e.key == "postStartCommand")
            .unwrap();
        assert_eq!(cmd.status, ReportStatus::Overridden);
        assert!(t.vcpus.is_none());
        assert!(t.post_start.is_none());
    }

    #[test]
    fn translate_features_known_to_profiles() {
        let f = parse(
            r#"{ "features": {
                "ghcr.io/devcontainers/features/rust:1": {},
                "node": {},
                "ghcr.io/devcontainers/features/docker-in-docker:2": {}
            } }"#,
        );
        let t = translate(&f, &TranslatorInputs::default(), Stage::Setup);
        assert!(t.profiles.contains(&"rust".to_string()));
        assert!(t.profiles.contains(&"node".to_string()));
        let docker = t
            .report
            .entries
            .iter()
            .find(|e| {
                e.key
                    .starts_with("features.ghcr.io/devcontainers/features/docker")
            })
            .unwrap();
        assert_eq!(docker.status, ReportStatus::Unsupported);
    }

    #[test]
    fn translate_remote_user_warns_unless_ubuntu() {
        let f = parse(r#"{ "remoteUser": "root" }"#);
        let t = translate(&f, &TranslatorInputs::default(), Stage::Start);
        let entry = t
            .report
            .entries
            .iter()
            .find(|e| e.key == "remoteUser")
            .unwrap();
        assert_eq!(entry.status, ReportStatus::Unsupported);

        let f = parse(r#"{ "remoteUser": "ubuntu" }"#);
        let t = translate(&f, &TranslatorInputs::default(), Stage::Start);
        let entry = t
            .report
            .entries
            .iter()
            .find(|e| e.key == "remoteUser")
            .unwrap();
        assert_eq!(entry.status, ReportStatus::Applied);
    }

    #[test]
    fn translate_unknown_keys_reported() {
        let f = parse(r#"{ "name": "x", "wackyKey": 1, "image": "ignored" }"#);
        let t = translate(&f, &TranslatorInputs::default(), Stage::Start);
        let wacky = t
            .report
            .entries
            .iter()
            .find(|e| e.key == "wackyKey")
            .unwrap();
        assert_eq!(wacky.status, ReportStatus::Unsupported);
        assert!(t.report.entries.iter().any(|e| e.key == "image"));
    }

    #[test]
    fn translate_forward_ports_numbers_and_strings() {
        let f = parse(r#"{ "forwardPorts": [3000, "8080:9090"] }"#);
        let t = translate(&f, &TranslatorInputs::default(), Stage::Start);
        assert_eq!(t.forward_ports.len(), 2);
        assert_eq!(t.forward_ports[0].guest.get(), 3000);
        assert_eq!(t.forward_ports[1].guest.get(), 8080);
        assert_eq!(t.forward_ports[1].host.get(), 9090);
    }

    #[test]
    fn translate_post_start_array_joins() {
        let f = parse(r#"{ "postStartCommand": ["echo a", "echo b"] }"#);
        let t = translate(&f, &TranslatorInputs::default(), Stage::Start);
        assert_eq!(t.post_start.as_deref(), Some("echo a && echo b"));
    }

    #[test]
    fn memory_spec_parses_binary_units() {
        assert_eq!(MemorySpec::parse("4GiB").unwrap().as_mib().as_u32(), 4096);
        assert_eq!(
            MemorySpec::parse("4096MiB").unwrap().as_mib().as_u32(),
            4096,
        );
        assert_eq!(MemorySpec::parse("512MiB").unwrap().as_mib().as_u32(), 512);
        assert_eq!(MemorySpec::parse("8GiB").unwrap().as_gib().as_u32(), 8);
    }

    #[test]
    fn memory_spec_parses_decimal_units() {
        // 1G = 1_000_000_000 B; ceil(1e9 / 2^20) = 954 MiB
        assert_eq!(MemorySpec::parse("1g").unwrap().as_mib().as_u32(), 954);
        // 512MB = 5.12e8 B; ceil(5.12e8 / 2^20) = 489 MiB
        assert_eq!(MemorySpec::parse("512mb").unwrap().as_mib().as_u32(), 489);
        // 4GB rounds to 3815 MiB
        assert_eq!(MemorySpec::parse("4GB").unwrap().as_mib().as_u32(), 3815);
    }

    #[test]
    fn memory_spec_accepts_bare_bytes() {
        // Bare integer = bytes; 1024 B rounds up to 1 MiB.
        assert_eq!(MemorySpec::parse("1024").unwrap().as_mib().as_u32(), 1);
        // 2 MiB worth of bytes survives the round-trip exactly.
        assert_eq!(
            MemorySpec::parse(&(2u64 * 1024 * 1024).to_string())
                .unwrap()
                .as_mib()
                .as_u32(),
            2,
        );
    }

    #[test]
    fn memory_spec_rejects_garbage() {
        assert!(MemorySpec::parse("abc").is_err());
        assert!(MemorySpec::parse("4 lemons").is_err());
        assert!(MemorySpec::parse("").is_err());
        // Zero is rejected at parse time so downstream code never sees it.
        assert!(MemorySpec::parse("0MiB").is_err());
        assert!(MemorySpec::parse("0").is_err());
    }

    #[test]
    fn memory_spec_as_gib_rounds_up_from_mib() {
        // 1500 MiB → ceil(1500/1024) = 2 GiB
        let spec = MemorySpec::parse("1500MiB").unwrap();
        assert_eq!(spec.as_mib().as_u32(), 1500);
        assert_eq!(spec.as_gib().as_u32(), 2);
    }

    #[test]
    fn memory_spec_fails_at_deserialize_for_bad_units() {
        // A bad memory value rejects the whole file at parse time, not at
        // translate time. The error path through serde carries the bad
        // string verbatim so the user can locate it.
        let err = ParsedDevcontainer::from_str(
            PathBuf::from("test.json"),
            r#"{ "hostRequirements": { "memory": "abc" } }"#,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("abc"),
            "expected error to mention bad value, got: {msg}"
        );
    }

    #[test]
    fn parse_mount_string_handles_docker_form() {
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp.path().to_str().unwrap();

        let docker = format!("type=bind,source={host},target=/b");
        let m = parse_mount_string(&docker).unwrap();
        assert_eq!(m.guest_path.to_string(), "/b");

        let no_type = format!("source={host},target=/b,readonly");
        let m = parse_mount_string(&no_type).unwrap();
        assert_eq!(m.guest_path.to_string(), "/b");

        assert!(parse_mount_string("type=volume,source=v,target=/b").is_err());
    }

    #[test]
    fn parse_mount_string_native_form_defers_to_mount_parse() {
        // Native `HOST[:GUEST]` form goes through `Mount::parse`, which
        // canonicalises the host path. Use a real directory so the call
        // succeeds, and check the guest path was preserved.
        let tmp = tempfile::tempdir().unwrap();
        let spec = format!("{}:/g", tmp.path().display());
        let m = parse_mount_string(&spec).unwrap();
        assert_eq!(m.guest_path.to_string(), "/g");
    }

    #[test]
    fn parse_mount_entry_object_form() {
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp.path().to_str().unwrap();
        let obj = serde_json::json!({"type": "bind", "source": host, "target": "/b"});
        let m = parse_mount_entry(&obj).unwrap();
        assert_eq!(m.guest_path.to_string(), "/b");
    }

    #[test]
    fn parse_mount_entry_rejects_unsupported_type() {
        let obj = serde_json::json!({"type": "volume", "source": "/v", "target": "/b"});
        let err = parse_mount_entry(&obj).unwrap_err();
        assert!(format!("{err:#}").contains("unsupported mount type"));
    }

    #[test]
    fn report_render_includes_path_and_columns() {
        let f = parse(r#"{ "hostRequirements": { "cpus": 4 } }"#);
        let t = translate(&f, &TranslatorInputs::default(), Stage::Start);
        let rendered = t.report.render();
        assert!(rendered.contains("test.json"));
        assert!(rendered.contains("hostRequirements.cpus"));
        assert!(rendered.contains("applied"));
    }

    #[test]
    fn pick_winner_prefers_workspace() {
        let found = vec![
            Discovered {
                path: PathBuf::from("/ws/.devcontainer/devcontainer.json"),
                origin: DiscoveryOrigin::Workspace,
            },
            Discovered {
                path: PathBuf::from("/m1/.devcontainer/devcontainer.json"),
                origin: DiscoveryOrigin::Mount,
            },
            Discovered {
                path: PathBuf::from("/m2/.devcontainer/devcontainer.json"),
                origin: DiscoveryOrigin::Mount,
            },
        ];
        let (winner, losers) = pick_winner(found).unwrap();
        assert_eq!(winner.origin, DiscoveryOrigin::Workspace);
        assert_eq!(losers.len(), 2);
    }

    #[test]
    fn pick_winner_falls_back_to_first_mount() {
        let found = vec![
            Discovered {
                path: PathBuf::from("/m1/.devcontainer/devcontainer.json"),
                origin: DiscoveryOrigin::Mount,
            },
            Discovered {
                path: PathBuf::from("/m2/.devcontainer/devcontainer.json"),
                origin: DiscoveryOrigin::Mount,
            },
        ];
        let (winner, losers) = pick_winner(found).unwrap();
        assert_eq!(
            winner.path,
            PathBuf::from("/m1/.devcontainer/devcontainer.json")
        );
        assert_eq!(
            losers,
            vec![PathBuf::from("/m2/.devcontainer/devcontainer.json")]
        );
    }

    #[test]
    fn translate_mounts_unsupported_with_workspace_flag() {
        let f = parse(r#"{ "mounts": [{ "type": "bind", "source": "/a", "target": "/b" }] }"#);
        let inputs = TranslatorInputs {
            cli_workspace_or_git_repo: true,
            ..TranslatorInputs::default()
        };
        let t = translate(&f, &inputs, Stage::Start);
        let row = t.report.entries.iter().find(|e| e.key == "mounts").unwrap();
        assert_eq!(row.status, ReportStatus::Unsupported);
        assert!(t.mounts.is_empty());
    }

    #[test]
    fn translate_container_env_overridden_by_cli() {
        let f = parse(r#"{ "containerEnv": { "FOO": "x", "BAR": "y" } }"#);
        let inputs = TranslatorInputs {
            cli_guest_env_keys: vec![EnvVarName::new("FOO").unwrap()],
            ..TranslatorInputs::default()
        };
        let t = translate(&f, &inputs, Stage::Start);
        let bar = EnvVarName::new("BAR").unwrap();
        let foo = EnvVarName::new("FOO").unwrap();
        assert_eq!(t.guest_env.get(&bar).map(String::as_str), Some("y"));
        assert!(!t.guest_env.contains_key(&foo));
        assert!(
            t.report
                .entries
                .iter()
                .any(|e| e.key == "containerEnv" && e.status == ReportStatus::Overridden)
        );
    }

    #[test]
    fn translate_container_env_reports_invalid_names() {
        let f = parse(r#"{ "containerEnv": { "GOOD": "1", "1BAD": "2", "AL SO": "3" } }"#);
        let t = translate(&f, &TranslatorInputs::default(), Stage::Start);
        assert_eq!(
            t.guest_env
                .get(&EnvVarName::new("GOOD").unwrap())
                .map(String::as_str),
            Some("1"),
        );
        assert_eq!(t.guest_env.len(), 1, "invalid keys must be dropped");
        let invalid = t
            .report
            .entries
            .iter()
            .find(|e| e.key == "containerEnv" && e.status == ReportStatus::Invalid)
            .unwrap();
        assert!(invalid.note.contains("1BAD"));
        assert!(invalid.note.contains("AL SO"));
    }
}
