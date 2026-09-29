//! Typed parsers for `coop-sandbox` JSON output (protocol 2).
//!
//! Runtime output is untrusted input: every record is parsed into a closed
//! type, identifiers are checked against what coop asked for, and the
//! effective VM configuration the isolation gate reads rejects unknown
//! fields, so a runtime that grows a new host-facing knob fails closed
//! instead of having it silently ignored.

use std::net::Ipv4Addr;

use anyhow::{Result, bail};
use serde::Deserialize;

use super::AppleError;
use super::state::{MachineName, OperationId};

/// Sandbox state as `coop-sandbox` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SandboxStatus {
    Running,
    /// The owner exists but its control channel is not answering yet.
    Booting,
    Stopped,
    /// The owner died without cleaning up; `start` recovers it.
    Crashed,
}

impl SandboxStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Booting => "booting",
            Self::Stopped => "stopped",
            Self::Crashed => "crashed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct VersionInfo {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) protocol: u32,
    pub(crate) containerization: String,
}

pub(crate) fn parse_version(json: &str) -> Result<VersionInfo> {
    serde_json::from_str(json).map_err(|e| {
        AppleError::RuntimeUnqualified(format!("`coop-sandbox version` output: {e}")).into()
    })
}

/// The runtime's durable record of a sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxRecord {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) image_reference: String,
    pub(crate) image_digest: String,
    pub(crate) cpus: u32,
    pub(crate) memory_bytes: u64,
    pub(crate) disk_bytes: u64,
    pub(crate) disk_generation: u64,
    /// The last `set`, `grow`, or `restore` the runtime committed, by the
    /// `--operation` id its caller passed. Absent before the first one.
    #[serde(default)]
    pub(crate) last_operation: Option<OperationId>,
}

impl SandboxRecord {
    /// Whether the runtime's last committed operation is `op`.
    pub(crate) fn committed(&self, op: &OperationId) -> bool {
        self.last_operation.as_ref() == Some(op)
    }

    /// The last committed operation, for messages.
    pub(crate) fn last_operation_label(&self) -> &str {
        self.last_operation
            .as_ref()
            .map_or("none", OperationId::as_str)
    }

    pub(crate) fn resources(&self) -> super::state::Resources {
        super::state::Resources {
            cpus: self.cpus,
            memory_bytes: self.memory_bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LiveState {
    pub(crate) pid: i32,
    pub(crate) ipv4: Option<Ipv4Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectiveMount {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) source: String,
    pub(crate) destination: String,
    pub(crate) options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct EffectiveInterface {
    /// CIDR, e.g. `10.231.4.2/24`.
    pub(crate) ipv4: String,
    pub(crate) ipv4_gateway: Option<String>,
    pub(crate) ipv6: Option<String>,
    pub(crate) network: String,
}

/// What the running VM was configured with, reported by its owner process.
/// Unknown fields are refused, so a new host-facing setting fails closed
/// until the gate checks it. `ipv6`, `ipv4_gateway`, `masked_paths`, and
/// `readonly_paths` are deliberately not checked: they sit on the verified
/// per-sandbox network or restrict only the guest, so no value exposes the
/// host.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Effective {
    pub(crate) id: String,
    pub(crate) image_reference: String,
    pub(crate) image_digest: String,
    pub(crate) cpus: u32,
    pub(crate) memory_bytes: u64,
    pub(crate) rootfs: EffectiveMount,
    pub(crate) mounts: Vec<EffectiveMount>,
    pub(crate) interfaces: Vec<EffectiveInterface>,
    pub(crate) socket_relays: u32,
    pub(crate) published_ports: u32,
    pub(crate) ssh_agent_forwarding: bool,
    pub(crate) masked_paths: Vec<String>,
    pub(crate) readonly_paths: Vec<String>,
    pub(crate) init_argv: Vec<String>,
    pub(crate) virtualization: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiskUsage {
    pub(crate) logical_bytes: u64,
    pub(crate) allocated_bytes: u64,
}

/// `coop-sandbox inspect <id>`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Inspect {
    pub(crate) record: SandboxRecord,
    pub(crate) status: SandboxStatus,
    #[serde(default)]
    pub(crate) live: Option<LiveState>,
    #[serde(default)]
    pub(crate) effective: Option<Effective>,
    pub(crate) disk: DiskUsage,
}

impl Inspect {
    pub(crate) fn ip(&self) -> Option<Ipv4Addr> {
        self.live.as_ref().and_then(|l| l.ipv4)
    }
}

pub(crate) fn parse_inspect(json: &str, expected: &MachineName) -> Result<Inspect> {
    let rec: Inspect = serde_json::from_str(json).map_err(|e| {
        AppleError::RuntimeUnqualified(format!("`coop-sandbox inspect` output: {e}"))
    })?;
    if rec.record.id != expected.as_str() {
        bail!(AppleError::IdentityConflict(format!(
            "inspect for {expected} returned sandbox {:?}",
            rec.record.id
        )));
    }
    if let Some(effective) = &rec.effective
        && effective.id != expected.as_str()
    {
        bail!(AppleError::IdentityConflict(format!(
            "sandbox {expected} reports an effective config for {:?}",
            effective.id
        )));
    }
    Ok(rec)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Listed {
    pub(crate) id: String,
    /// Parsed so an unknown status fails the listing closed.
    pub(crate) status: SandboxStatus,
}

/// `coop-sandbox list`. Duplicate ids are a protocol error, never merged.
pub(crate) fn parse_list(json: &str) -> Result<Vec<Listed>> {
    let listed: Vec<Listed> = serde_json::from_str(json)
        .map_err(|e| AppleError::RuntimeUnqualified(format!("`coop-sandbox list` output: {e}")))?;
    let mut seen = std::collections::HashSet::new();
    for l in &listed {
        if !seen.insert(l.id.as_str()) {
            bail!(AppleError::RuntimeUnqualified(format!(
                "`coop-sandbox list` reported {} twice",
                l.id
            )));
        }
    }
    Ok(listed)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ImageEntry {
    pub(crate) reference: String,
    pub(crate) digest: String,
}

/// `coop-sandbox image list` and `image import`.
pub(crate) fn parse_images(json: &str) -> Result<Vec<ImageEntry>> {
    let images: Vec<ImageEntry> = serde_json::from_str(json)
        .map_err(|e| AppleError::RuntimeUnqualified(format!("`coop-sandbox image` output: {e}")))?;
    for image in &images {
        let valid = image
            .digest
            .strip_prefix("sha256:")
            .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        if !valid {
            bail!(AppleError::RuntimeUnqualified(format!(
                "image {} has an invalid digest {:?}",
                image.reference, image.digest
            )));
        }
    }
    Ok(images)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiskEntry {
    pub(crate) name: String,
    pub(crate) logical_bytes: u64,
}

/// `coop-sandbox disk list` and `commit`.
pub(crate) fn parse_disks(json: &str) -> Result<Vec<DiskEntry>> {
    serde_json::from_str(json).map_err(|e| {
        AppleError::RuntimeUnqualified(format!("`coop-sandbox disk` output: {e}")).into()
    })
}

pub(crate) fn parse_disk(json: &str) -> Result<DiskEntry> {
    serde_json::from_str(json).map_err(|e| {
        AppleError::RuntimeUnqualified(format!("`coop-sandbox commit` output: {e}")).into()
    })
}

/// The disk maintenance VMs boot from, as `coop-sandbox maintenance
/// inspect` and `maintenance install` report it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MaintenanceArtifact {
    pub(crate) version: String,
    pub(crate) reference: String,
    pub(crate) digest: String,
}

/// Output of `coop-sandbox maintenance inspect` or `install`; `null` means
/// none is installed.
pub(crate) fn parse_maintenance(json: &str) -> Result<Option<MaintenanceArtifact>> {
    serde_json::from_str(json).map_err(|e| {
        AppleError::RuntimeUnqualified(format!("`coop-sandbox maintenance` output: {e}")).into()
    })
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/coop-sandbox");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap()
    }

    fn sandbox() -> MachineName {
        MachineName::new("coop-0a1b2c3d-00112233445566ff").unwrap()
    }

    #[test]
    fn maintenance_artifact_parses_or_is_absent() {
        assert_eq!(parse_maintenance("null").unwrap(), None);
        let installed = parse_maintenance(
            r#"{"version":"1","reference":"local/m:1","digest":"sha256:ab","capacityBytes":1,"installedAt":"2026-09-26T00:00:00Z","disk":"tools-ab-1.ext4"}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(installed.version, "1");
        assert!(parse_maintenance("{}").is_err());
    }

    #[test]
    fn version_parses() {
        let v = parse_version(&fixture("version.json")).unwrap();
        assert_eq!(v.name, "coop-sandbox");
        assert_eq!(v.protocol, 2);
        assert_eq!(v.containerization, "0.45.0");
        assert!(parse_version("{\"name\":\"x\"}").is_err());
    }

    #[test]
    fn running_inspect_parses_live_and_effective() {
        let rec = parse_inspect(&fixture("inspect-running.json"), &sandbox()).unwrap();
        assert_eq!(rec.status, SandboxStatus::Running);
        assert_eq!(rec.ip(), Some(Ipv4Addr::new(10, 231, 2, 2)));
        let eff = rec.effective.unwrap();
        assert_eq!(eff.mounts.len(), 7);
        assert_eq!(eff.interfaces.len(), 1);
        assert_eq!(rec.record.disk_generation, 0);
    }

    #[test]
    fn stopped_inspect_has_no_live_state() {
        let rec = parse_inspect(&fixture("inspect-stopped.json"), &sandbox()).unwrap();
        assert_eq!(rec.status, SandboxStatus::Stopped);
        assert!(rec.live.is_none() && rec.effective.is_none());
    }

    #[test]
    fn inspect_rejects_other_ids_and_unknown_effective_fields() {
        let other = MachineName::new("coop-0a1b2c3d-ffffffffffffffff").unwrap();
        let err = parse_inspect(&fixture("inspect-running.json"), &other).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<AppleError>(),
            Some(AppleError::IdentityConflict(_))
        ));

        let widened = fixture("inspect-running.json").replace(
            "\"socketRelays\" : 0,",
            "\"socketRelays\" : 0,\n    \"hostShares\" : [\"/Users\"],",
        );
        assert_ne!(widened, fixture("inspect-running.json"));
        let err = parse_inspect(&widened, &sandbox()).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<AppleError>(),
            Some(AppleError::RuntimeUnqualified(_))
        ));
        assert!(parse_inspect("{}", &sandbox()).is_err());
    }

    #[test]
    fn list_rejects_duplicates_and_unknown_status() {
        let one = r#"[{"id":"a","status":"running","owner":"o"}]"#;
        assert_eq!(parse_list(one).unwrap().len(), 1);
        let dup = r#"[{"id":"a","status":"running","owner":"o"},{"id":"a","status":"stopped","owner":"o"}]"#;
        assert!(parse_list(dup).is_err());
        assert!(parse_list(r#"[{"id":"a","status":"paused","owner":"o"}]"#).is_err());
    }

    #[test]
    fn status_labels_are_the_runtime_vocabulary() {
        for status in [
            SandboxStatus::Running,
            SandboxStatus::Booting,
            SandboxStatus::Stopped,
            SandboxStatus::Crashed,
        ] {
            let parsed: SandboxStatus =
                serde_json::from_str(&format!("\"{}\"", status.label())).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn image_digests_are_strict() {
        let good = format!(
            r#"[{{"reference":"r","digest":"sha256:{}"}}]"#,
            "a".repeat(64)
        );
        assert_eq!(parse_images(&good).unwrap().len(), 1);
        assert!(parse_images(r#"[{"reference":"r","digest":"sha256:abc"}]"#).is_err());
        assert!(parse_images(r#"[{"reference":"r","digest":"md5:00"}]"#).is_err());
    }
}
