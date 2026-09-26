//! Runtime qualification and the isolation gate.
//!
//! coop drives `coop-sandbox` (`macos/coop-sandbox`), a runtime built for it
//! on `apple/containerization`: one VM per sandbox, each on its own vmnet
//! network, with no host mounts, socket relays, published ports, or SSH-agent
//! forwarding. Its record type cannot express those, but coop still verifies
//! the *effective* configuration the running VM's owner reports before every
//! hand-out, and refuses a runtime whose protocol it does not know. No flag or
//! config key relaxes these checks.

use std::net::Ipv4Addr;
use std::path::Path;

use anyhow::{Result, bail};

use super::AppleError;
use super::protocol::{Effective, EffectiveMount, Inspect, SandboxStatus, VersionInfo};
use super::state::{MachineName, OwnerId, Resources};

/// The protocol and containerization release this build was validated with.
pub(crate) const PROTOCOL: u32 = 2;
pub(crate) const CONTAINERIZATION: &str = "0.45.0";

/// Kernel pseudo-filesystems a sandbox may mount, as (type, source,
/// destination). Nothing else — no share, bind, or block device from the host.
const ALLOWED_MOUNTS: &[(&str, &str, &str)] = &[
    ("proc", "proc", "/proc"),
    ("sysfs", "sysfs", "/sys"),
    ("devtmpfs", "none", "/dev"),
    ("mqueue", "mqueue", "/dev/mqueue"),
    ("tmpfs", "tmpfs", "/dev/shm"),
    ("cgroup2", "none", "/sys/fs/cgroup"),
    ("devpts", "devpts", "/dev/pts"),
];

/// What `qualify` learned about the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualifiedRuntime {
    /// `coop-sandbox <version> (containerization <version>)`, for diagnostics.
    pub(crate) identity: String,
}

/// Accept only the runtime protocol and containerization release coop was
/// validated against.
pub(crate) fn qualify(info: &VersionInfo) -> Result<QualifiedRuntime> {
    let identity = format!(
        "{} {} (containerization {}, protocol {})",
        info.name, info.version, info.containerization, info.protocol
    );
    if info.name != "coop-sandbox" {
        bail!(AppleError::RuntimeUnqualified(format!(
            "{identity} is not coop-sandbox; `[apple_container] binary` must point at the \
             runtime built by scripts/build-coop-sandbox.sh"
        )));
    }
    if info.protocol != PROTOCOL || info.containerization != CONTAINERIZATION {
        bail!(AppleError::RuntimeUnqualified(format!(
            "{identity} is not the qualified runtime (protocol {PROTOCOL}, containerization \
             {CONTAINERIZATION}); rebuild it from this checkout with scripts/build-coop-sandbox.sh"
        )));
    }
    Ok(QualifiedRuntime { identity })
}

/// What the gate compares the runtime's report against: the values coop
/// recorded when it created the sandbox.
pub(crate) struct Expected<'a> {
    pub(crate) sandbox: &'a MachineName,
    pub(crate) owner: &'a OwnerId,
    pub(crate) runtime_root: &'a Path,
    pub(crate) resources: Resources,
}

/// Where the runtime keeps `sandbox`'s disk under its (canonical) root.
pub(crate) fn rootfs_path(runtime_root: &Path, sandbox: &MachineName) -> std::path::PathBuf {
    runtime_root
        .join("sandboxes")
        .join(sandbox.as_str())
        .join("rootfs.ext4")
}

/// Pre-boot check of the runtime's persisted record.
pub(crate) fn verify_record(inspect: &Inspect, expected: &Expected<'_>) -> Result<()> {
    let r = &inspect.record;
    if r.id != expected.sandbox.as_str() || r.owner != expected.owner.as_str() {
        bail!(AppleError::IdentityConflict(format!(
            "sandbox {} is recorded for owner {:?}, not this installation",
            expected.sandbox,
            super::cli::sanitize_for_display(&r.owner)
        )));
    }
    if r.resources() != expected.resources {
        bail!(AppleError::IdentityConflict(format!(
            "sandbox {} records {}, expected {}",
            expected.sandbox,
            r.resources(),
            expected.resources
        )));
    }
    Ok(())
}

/// Process-local proof that a sandbox's *current* boot passed the isolation
/// gate. Fields are private and it is neither serializable nor cloneable, so
/// it cannot be persisted or forged; it is re-established after every boot
/// and before every SSH target is handed out.
#[derive(Debug)]
pub(crate) struct SecurityReady {
    sandbox: MachineName,
    owner_pid: i32,
    ip: Ipv4Addr,
}

impl SecurityReady {
    pub(crate) fn sandbox(&self) -> &MachineName {
        &self.sandbox
    }

    /// PID of the owner process that holds this boot's VM; a different PID
    /// later means the sandbox restarted.
    pub(crate) fn owner_pid(&self) -> i32 {
        self.owner_pid
    }

    pub(crate) fn ip(&self) -> Ipv4Addr {
        self.ip
    }
}

/// Post-boot check of the effective configuration the owner reports.
pub(crate) fn verify_effective(
    inspect: &Inspect,
    expected: &Expected<'_>,
) -> Result<SecurityReady> {
    verify_record(inspect, expected)?;
    let name = expected.sandbox;
    if inspect.status != SandboxStatus::Running {
        bail!(AppleError::OperationUncertain(format!(
            "sandbox {name} is {}, not running",
            inspect.status.label()
        )));
    }
    let (Some(live), Some(eff)) = (&inspect.live, &inspect.effective) else {
        bail!(AppleError::RuntimeUnqualified(format!(
            "running sandbox {name} reports no live state or effective configuration"
        )));
    };
    let Some(ip) = live.ipv4 else {
        bail!(AppleError::NetworkIsolation(format!(
            "sandbox {name} reports no address"
        )));
    };
    let running = Resources {
        cpus: eff.cpus,
        memory_bytes: eff.memory_bytes,
    };
    if running != expected.resources {
        bail!(AppleError::IdentityConflict(format!(
            "sandbox {name} runs with {running}, expected {}",
            expected.resources
        )));
    }
    if eff.image_digest != inspect.record.image_digest {
        bail!(AppleError::IdentityConflict(format!(
            "sandbox {name} booted {} but records {}",
            eff.image_digest, inspect.record.image_digest
        )));
    }
    verify_host_exposure(name, eff, expected.runtime_root)?;
    verify_network(name, eff, ip)?;
    if eff.init_argv != ["/sbin/init"] || eff.virtualization {
        bail!(AppleError::HostExposure(format!(
            "sandbox {name} runs {:?} with nested virtualization {}; expected /sbin/init without it",
            eff.init_argv, eff.virtualization
        )));
    }
    Ok(SecurityReady {
        sandbox: name.clone(),
        owner_pid: live.pid,
        ip,
    })
}

fn verify_host_exposure(name: &MachineName, eff: &Effective, runtime_root: &Path) -> Result<()> {
    if eff.ssh_agent_forwarding {
        bail!(AppleError::HostExposure(format!(
            "sandbox {name} forwards the host SSH agent"
        )));
    }
    if eff.socket_relays != 0 || eff.published_ports != 0 {
        bail!(AppleError::HostExposure(format!(
            "sandbox {name} relays {} sockets and publishes {} ports; expected none",
            eff.socket_relays, eff.published_ports
        )));
    }
    let expected_rootfs = rootfs_path(runtime_root, name);
    if eff.rootfs.kind != "ext4" || Path::new(&eff.rootfs.source) != expected_rootfs {
        bail!(AppleError::HostExposure(format!(
            "sandbox {name} boots from {} ({}), not its own disk {}",
            super::cli::sanitize_for_display(&eff.rootfs.source),
            super::cli::sanitize_for_display(&eff.rootfs.kind),
            expected_rootfs.display()
        )));
    }
    for mount in &eff.mounts {
        if !is_allowed_mount(mount) {
            bail!(AppleError::HostExposure(format!(
                "sandbox {name} mounts {} from {} at {}; only kernel pseudo-filesystems are allowed",
                super::cli::sanitize_for_display(&mount.kind),
                super::cli::sanitize_for_display(&mount.source),
                super::cli::sanitize_for_display(&mount.destination)
            )));
        }
    }
    Ok(())
}

fn is_allowed_mount(mount: &EffectiveMount) -> bool {
    ALLOWED_MOUNTS.iter().any(|(kind, source, dest)| {
        mount.kind == *kind && mount.source == *source && mount.destination == *dest
    })
}

/// Exactly one interface, on a per-sandbox vmnet network, carrying the
/// address the owner reports.
fn verify_network(name: &MachineName, eff: &Effective, ip: Ipv4Addr) -> Result<()> {
    let [iface] = eff.interfaces.as_slice() else {
        bail!(AppleError::NetworkIsolation(format!(
            "sandbox {name} has {} network interfaces; expected exactly one",
            eff.interfaces.len()
        )));
    };
    let addr = iface
        .ipv4
        .split('/')
        .next()
        .and_then(|a| a.parse::<Ipv4Addr>().ok());
    let subnet_ok = iface
        .network
        .strip_prefix("vmnet-shared:10.231.")
        .and_then(|rest| rest.strip_suffix(".0/24"))
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| addr.is_some_and(|a| a.octets()[..3] == [10, 231, n]));
    if addr != Some(ip) || !subnet_ok {
        bail!(AppleError::NetworkIsolation(format!(
            "sandbox {name} interface {} on {} does not match its dedicated network address {ip}",
            super::cli::sanitize_for_display(&iface.ipv4),
            super::cli::sanitize_for_display(&iface.network)
        )));
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::apple_container::protocol::{parse_inspect, parse_version};

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/coop-sandbox");
    fn owner() -> OwnerId {
        OwnerId::try_from("0a1b2c3d00112233445566778899aabb".to_string()).unwrap()
    }
    const ROOT: &str = "/Users/me/.coop-apple/backends/apple-container-v1/runtime";

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap()
    }

    fn sandbox() -> MachineName {
        MachineName::new("coop-0a1b2c3d-00112233445566ff").unwrap()
    }

    fn expected<'a>(name: &'a MachineName, owner: &'a OwnerId) -> Expected<'a> {
        Expected {
            sandbox: name,
            owner,
            runtime_root: Path::new(ROOT),
            resources: Resources {
                cpus: 2,
                memory_bytes: 2048 * 1024 * 1024,
            },
        }
    }

    fn kind(err: &anyhow::Error) -> &AppleError {
        err.downcast_ref::<AppleError>().unwrap()
    }

    type Mutation = fn(&mut serde_json::Value);
    /// (label, mutation of the running fixture, expected error class).
    type Case = (&'static str, Mutation, fn(&AppleError) -> bool);

    fn gate(json: &str) -> Result<SecurityReady> {
        let name = sandbox();
        let inspect = parse_inspect(json, &name)?;
        verify_effective(&inspect, &expected(&name, &owner()))
    }

    /// The runtime reports the values its sources declare, and `Package.swift`
    /// pins the same containerization release, so a bump on either side
    /// must be mirrored here.
    #[test]
    fn pins_match_the_runtime_sources() {
        let layout = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/macos/coop-sandbox/Sources/CoopSandboxCore/Layout.swift"
        ));
        let package = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/macos/coop-sandbox/Package.swift"
        ));
        let protocol = format!("public let protocolVersion = {PROTOCOL}\n");
        let layout_pin = format!("public let containerizationVersion = \"{CONTAINERIZATION}\"\n");
        let package_pin = format!(
            "\"https://github.com/apple/containerization.git\", exact: \"{CONTAINERIZATION}\")"
        );
        assert!(
            layout.contains(&protocol),
            "Layout.swift lacks {protocol:?}"
        );
        assert!(
            layout.contains(&layout_pin),
            "Layout.swift lacks {layout_pin:?}"
        );
        assert!(
            package.contains(&package_pin),
            "Package.swift lacks {package_pin:?}"
        );
    }

    #[test]
    fn qualifies_only_the_validated_runtime() {
        let v = parse_version(&fixture("version.json")).unwrap();
        assert!(qualify(&v).unwrap().identity.contains("coop-sandbox 0.2.0"));
        for (field, value) in [
            ("protocol", "1"),
            ("containerization", "\"0.47.0\""),
            ("name", "\"container\""),
        ] {
            let mut j: serde_json::Value = serde_json::from_str(&fixture("version.json")).unwrap();
            j[field] = serde_json::from_str(value).unwrap();
            let err = qualify(&parse_version(&j.to_string()).unwrap()).unwrap_err();
            assert!(
                matches!(kind(&err), AppleError::RuntimeUnqualified(_)),
                "{field}"
            );
        }
    }

    #[test]
    fn gate_accepts_the_real_running_shape() {
        let ready = gate(&fixture("inspect-running.json")).unwrap();
        assert_eq!(ready.ip(), Ipv4Addr::new(10, 231, 2, 2));
        assert_eq!(ready.sandbox(), &sandbox());
    }

    #[test]
    fn gate_rejects_a_stopped_sandbox() {
        let err = gate(&fixture("inspect-stopped.json")).unwrap_err();
        assert!(matches!(kind(&err), AppleError::OperationUncertain(_)));
    }

    fn host_exposure_cases() -> Vec<Case> {
        vec![
            (
                "agent",
                |j| j["effective"]["sshAgentForwarding"] = true.into(),
                |e| matches!(e, AppleError::HostExposure(_)),
            ),
            (
                "relay",
                |j| j["effective"]["socketRelays"] = 1.into(),
                |e| matches!(e, AppleError::HostExposure(_)),
            ),
            (
                "port",
                |j| j["effective"]["publishedPorts"] = 1.into(),
                |e| matches!(e, AppleError::HostExposure(_)),
            ),
            (
                "virtiofs",
                |j| {
                    j["effective"]["mounts"].as_array_mut().unwrap().push(serde_json::json!({
                        "type": "virtiofs", "source": "/Users/me", "destination": "/proc", "options": []
                    }));
                },
                |e| matches!(e, AppleError::HostExposure(_)),
            ),
            (
                "host path at allowed destination",
                |j| j["effective"]["mounts"][0]["source"] = "/Users/me".into(),
                |e| matches!(e, AppleError::HostExposure(_)),
            ),
            (
                "foreign rootfs",
                |j| j["effective"]["rootfs"]["source"] = "/Users/me/disk.img".into(),
                |e| matches!(e, AppleError::HostExposure(_)),
            ),
            (
                "init",
                |j| j["effective"]["initArgv"] = serde_json::json!(["/bin/sh"]),
                |e| matches!(e, AppleError::HostExposure(_)),
            ),
            (
                "nested virt",
                |j| j["effective"]["virtualization"] = true.into(),
                |e| matches!(e, AppleError::HostExposure(_)),
            ),
        ]
    }

    fn network_and_identity_cases() -> Vec<Case> {
        vec![
            (
                "second interface",
                |j| {
                    let first = j["effective"]["interfaces"][0].clone();
                    j["effective"]["interfaces"]
                        .as_array_mut()
                        .unwrap()
                        .push(first);
                },
                |e| matches!(e, AppleError::NetworkIsolation(_)),
            ),
            (
                "shared network",
                |j| {
                    j["effective"]["interfaces"][0]["network"] =
                        "vmnet-shared:192.168.64.0/24".into();
                },
                |e| matches!(e, AppleError::NetworkIsolation(_)),
            ),
            (
                "rootfs kind",
                |j| j["effective"]["rootfs"]["type"] = "none".into(),
                |e| matches!(e, AppleError::HostExposure(_)),
            ),
            (
                "address outside subnet",
                |j| {
                    j["live"]["ipv4"] = "10.231.9.2".into();
                    j["effective"]["interfaces"][0]["ipv4"] = "10.231.9.2/24".into();
                },
                |e| matches!(e, AppleError::NetworkIsolation(_)),
            ),
            (
                "no live state",
                |j| j["live"] = serde_json::Value::Null,
                |e| matches!(e, AppleError::RuntimeUnqualified(_)),
            ),
            (
                "record memory",
                |j| j["record"]["memoryBytes"] = 1.into(),
                |e| matches!(e, AppleError::IdentityConflict(_)),
            ),
            (
                "address mismatch",
                |j| j["live"]["ipv4"] = "10.231.2.9".into(),
                |e| matches!(e, AppleError::NetworkIsolation(_)),
            ),
            (
                "owner",
                |j| j["record"]["owner"] = "ffffffffffffffffffffffffffffffff".into(),
                |e| matches!(e, AppleError::IdentityConflict(_)),
            ),
            (
                "cpus",
                |j| j["effective"]["cpus"] = 8.into(),
                |e| matches!(e, AppleError::IdentityConflict(_)),
            ),
            (
                "image",
                |j| j["effective"]["imageDigest"] = format!("sha256:{}", "f".repeat(64)).into(),
                |e| matches!(e, AppleError::IdentityConflict(_)),
            ),
        ]
    }

    /// Each mutation of the real running shape must fail with its class.
    #[test]
    fn gate_rejects_host_exposure_network_and_identity_changes() {
        let base: serde_json::Value =
            serde_json::from_str(&fixture("inspect-running.json")).unwrap();
        for (label, mutate, expect) in host_exposure_cases()
            .into_iter()
            .chain(network_and_identity_cases())
        {
            let mut j = base.clone();
            mutate(&mut j);
            let err = gate(&j.to_string()).unwrap_err();
            assert!(expect(kind(&err)), "{label}: {err:#}");
        }
    }
}
