//! Machine-readable (`--json`) output shared across read/query commands.
//!
//! Every command that supports `--json` builds a single `Serialize` view
//! model and hands it to [`render_json`]; the human formatter reads the
//! same value, so the two representations cannot disagree about the data.
//! See `docs/json-output-design.md` for the full design.

use std::io::Write as _;

use serde::Serialize;

use crate::{backend, config, devcontainer, guest};

/// Serialize `value` as pretty JSON to stdout, with a trailing newline.
///
/// This is the only place JSON output reaches stdout; tracing and human
/// decoration stay on stderr so `coop … --json | jq` sees clean JSON.
pub(crate) fn render_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let mut out = std::io::stdout();
    serde_json::to_writer_pretty(&mut out, value)
        .map_err(|e| anyhow::anyhow!("Failed to write JSON: {e}"))?;
    writeln!(out).map_err(|e| anyhow::anyhow!("Failed to write JSON: {e}"))?;
    Ok(())
}

/// Instance run state — the closed set the human path prints as
/// `"running"` / `"stopped"`, or `"unknown"` in a listing when the backend
/// could not probe one instance.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InstanceState {
    Running,
    Stopped,
    /// The backend could not determine the state (listings only).
    Unknown,
}

impl InstanceState {
    /// Project a backend `is_running` boolean into the enum.
    pub(crate) fn from_running(running: bool) -> Self {
        if running {
            Self::Running
        } else {
            Self::Stopped
        }
    }

    /// Human-readable label — the same tokens the text output prints.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }
}

/// Which VM backend is in use. Like [`backend::PlatformBackend`] and
/// [`crate::secret_store::Backend`], the variants are `#[cfg]`-gated per
/// OS and feature set — only this build's backend can ever be selected, so
/// only its token (`"firecracker"` on Linux, `"lima"` on macOS, or
/// `"apple-container"` with that feature) is ever serialized.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BackendKind {
    #[cfg(not(target_os = "macos"))]
    Firecracker,
    #[cfg(all(target_os = "macos", not(feature = "apple-container")))]
    Lima,
    #[cfg(all(target_os = "macos", feature = "apple-container"))]
    AppleContainer,
}

impl BackendKind {
    /// The backend kind for this build. `PlatformBackend` is a compile-time
    /// type alias, so this is fixed per target OS and feature set.
    pub(crate) fn of(_be: &backend::PlatformBackend) -> Self {
        #[cfg(all(target_os = "macos", feature = "apple-container"))]
        {
            Self::AppleContainer
        }
        #[cfg(all(target_os = "macos", not(feature = "apple-container")))]
        {
            Self::Lima
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::Firecracker
        }
    }
}

// ── status / list ────────────────────────────────────────────

/// `coop status --json` single-instance / array element. `usage` is
/// `None` for a stopped instance or when the SSH usage query failed on a
/// running guest.
#[derive(Serialize)]
pub(crate) struct InstanceStatus<'a> {
    pub name: &'a config::InstanceName,
    pub state: InstanceState,
    pub image: &'a config::ImageName,
    pub backend: BackendKind,
    pub usage: Option<backend::ResourceUsage>,
}

/// `coop list --json` array element — deliberately smaller than
/// [`InstanceStatus`]: `list` never runs the per-instance usage query.
#[derive(Serialize)]
pub(crate) struct InstanceSummary<'a> {
    pub name: &'a config::InstanceName,
    pub state: InstanceState,
}

// ── images ───────────────────────────────────────────────────

/// `coop images --json` array element. The human path collapses absence
/// to `"none"` / `"unknown"`; JSON models it as `[]` / `null`, and emits
/// the raw byte count instead of a formatted GiB string.
#[derive(Serialize)]
pub(crate) struct ImageInfo<'a> {
    pub name: &'a config::ImageName,
    pub profiles: &'a [String],
    pub created: Option<&'a str>,
    /// `None` when the backend keeps image content outside coop's data dir.
    pub size_bytes: Option<u64>,
}

// ── profiles list ────────────────────────────────────────────

/// `coop profiles --json`.
#[derive(Serialize)]
pub(crate) struct ProfilesList {
    pub builtin: Vec<ProfileEntry>,
    pub custom: Vec<ProfileEntry>,
}

/// One profile row. `name` is `String` because profile names have no
/// newtype; `summary` is the same free-text the human path prints.
#[derive(Serialize)]
pub(crate) struct ProfileEntry {
    pub name: String,
    pub summary: String,
}

// ── up / start --dry-run ─────────────────────────────────────

/// VM resource overrides resolved for a dry-run. Each is `None` when the
/// flag was not given. The `MiB`/`GiB` newtypes serialize as bare numbers.
#[derive(Serialize)]
pub(crate) struct VmOverrides {
    pub vcpus: Option<u8>,
    pub mem_mib: Option<config::MiB>,
    pub disk_gib: Option<config::GiB>,
}

/// `coop up --dry-run --json` / `coop start --dry-run --json`. `report`
/// is `None` when no devcontainer.json applied.
#[derive(Serialize)]
pub(crate) struct DryRunPlan<'a> {
    pub report: Option<&'a devcontainer::Report>,
    pub profiles: &'a [String],
    pub guest_user: &'a guest::GuestUser,
    pub vm: VmOverrides,
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code — panics are assertions"
)]
mod tests {
    use super::{
        BackendKind, DryRunPlan, ImageInfo, InstanceState, InstanceStatus, InstanceSummary,
        ProfileEntry, ProfilesList, VmOverrides,
    };
    use crate::{backend, config, devcontainer, guest};
    use serde_json::{Value, json};

    fn to_value<T: serde::Serialize>(v: &T) -> Value {
        serde_json::to_value(v).expect("view model serializes")
    }

    #[test]
    fn instance_state_tokens() {
        assert_eq!(to_value(&InstanceState::Running), json!("running"));
        assert_eq!(to_value(&InstanceState::Stopped), json!("stopped"));
        assert_eq!(InstanceState::from_running(true), InstanceState::Running);
        assert_eq!(InstanceState::from_running(false), InstanceState::Stopped);
        assert_eq!(InstanceState::Running.label(), "running");
        assert_eq!(InstanceState::Stopped.label(), "stopped");
    }

    /// The backend token for the host — the only variant that exists in
    /// this build.
    fn platform_backend_token() -> &'static str {
        if cfg!(all(target_os = "macos", feature = "apple-container")) {
            "apple-container"
        } else if cfg!(target_os = "macos") {
            "lima"
        } else {
            "firecracker"
        }
    }

    #[test]
    fn backend_kind_token_matches_platform() {
        let be = backend::PlatformBackend::new();
        assert_eq!(
            to_value(&BackendKind::of(&be)),
            json!(platform_backend_token())
        );
    }

    #[test]
    fn instance_status_running_with_usage() {
        let name = config::InstanceName::new("web").unwrap();
        let image = config::default_image_name();
        let view = InstanceStatus {
            name: &name,
            state: InstanceState::Running,
            image: &image,
            backend: BackendKind::of(&backend::PlatformBackend::new()),
            usage: Some(backend::ResourceUsage {
                load_1m: 0.12,
                mem_used_mib: 512,
                mem_total_mib: 2048,
                disk_used_mib: 8192,
                disk_total_mib: 20480,
            }),
        };
        assert_eq!(
            to_value(&view),
            json!({
                "name": "web",
                "state": "running",
                "image": "default",
                "backend": platform_backend_token(),
                "usage": {
                    "load_1m": 0.12,
                    "mem_used_mib": 512,
                    "mem_total_mib": 2048,
                    "disk_used_mib": 8192,
                    "disk_total_mib": 20480
                }
            })
        );
    }

    #[test]
    fn instance_status_stopped_has_null_usage() {
        let name = config::InstanceName::new("db").unwrap();
        let image = config::default_image_name();
        let view = InstanceStatus {
            name: &name,
            state: InstanceState::Stopped,
            image: &image,
            backend: BackendKind::of(&backend::PlatformBackend::new()),
            usage: None,
        };
        assert_eq!(to_value(&view)["usage"], Value::Null);
        assert_eq!(to_value(&view)["state"], json!("stopped"));
    }

    #[test]
    fn instance_summary_shape() {
        let name = config::InstanceName::new("web").unwrap();
        let view = InstanceSummary {
            name: &name,
            state: InstanceState::Running,
        };
        assert_eq!(
            to_value(&view),
            json!({ "name": "web", "state": "running" })
        );
    }

    #[test]
    fn image_info_empty_profiles_and_null_created() {
        let name = config::default_image_name();
        let view = ImageInfo {
            name: &name,
            profiles: &[],
            created: None,
            size_bytes: None,
        };
        assert_eq!(
            to_value(&view),
            json!({ "name": "default", "profiles": [], "created": Value::Null, "size_bytes": Value::Null })
        );
    }

    #[test]
    fn image_info_with_profiles_and_created() {
        let name = config::default_image_name();
        let profiles = vec!["python".to_string(), "node".to_string()];
        let view = ImageInfo {
            name: &name,
            profiles: &profiles,
            created: Some("2026-06-01T12:00:00Z"),
            size_bytes: Some(8_589_934_592),
        };
        assert_eq!(
            to_value(&view),
            json!({
                "name": "default",
                "profiles": ["python", "node"],
                "created": "2026-06-01T12:00:00Z",
                "size_bytes": 8_589_934_592u64
            })
        );
    }

    #[test]
    fn profiles_list_shape() {
        let view = ProfilesList {
            builtin: vec![ProfileEntry {
                name: "python".to_string(),
                summary: "python3".to_string(),
            }],
            custom: Vec::new(),
        };
        assert_eq!(
            to_value(&view),
            json!({
                "builtin": [{ "name": "python", "summary": "python3" }],
                "custom": []
            })
        );
    }

    #[test]
    fn dry_run_plan_null_report_and_number_overrides() {
        let user = guest::GuestUser::default();
        let profiles = vec!["node".to_string()];
        let view = DryRunPlan {
            report: None,
            profiles: &profiles,
            guest_user: &user,
            vm: VmOverrides {
                vcpus: Some(4),
                mem_mib: config::MiB::new(2048),
                disk_gib: config::GiB::new(50),
            },
        };
        assert_eq!(
            to_value(&view),
            json!({
                "report": Value::Null,
                "profiles": ["node"],
                "guest_user": "ubuntu",
                "vm": { "vcpus": 4, "mem_mib": 2048, "disk_gib": 50 }
            })
        );
    }

    #[test]
    fn dry_run_plan_with_report() {
        use devcontainer::{Report, ReportSource, ReportStatus};
        let mut report = Report::default();
        report.push(
            "hostRequirements.cpus",
            ReportStatus::Applied,
            ReportSource::Devcontainer,
            "4",
            "",
        );
        let user = guest::GuestUser::default();
        let view = DryRunPlan {
            report: Some(&report),
            profiles: &[],
            guest_user: &user,
            vm: VmOverrides {
                vcpus: None,
                mem_mib: None,
                disk_gib: None,
            },
        };
        let v = to_value(&view);
        assert_eq!(v["report"]["entries"][0]["status"], json!("applied"));
        assert_eq!(v["report"]["entries"][0]["source"], json!("devcontainer"));
        assert_eq!(
            v["report"]["entries"][0]["key"],
            json!("hostRequirements.cpus")
        );
        assert_eq!(v["vm"]["vcpus"], Value::Null);
    }
}
