//! Versioned, atomic selection of the confined Linux Firecracker launcher.

use std::ffi::OsStr;
use std::io::Read as _;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::Instance;
use crate::fs_util::PrivateDir;

const STATE_FILE: &str = "firecracker.runtime.json";
const JAILED_V1: &str = "{\"version\":1,\"launcher\":\"jailer\"}";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeState {
    version: u8,
    launcher: String,
}

fn parse_state(content: &str) -> Result<()> {
    let state: RuntimeState =
        serde_json::from_str(content).context("Cannot parse Firecracker runtime state")?;
    if state.version != 1 || state.launcher != "jailer" {
        bail!("Unsupported Firecracker runtime state; do not start this instance");
    }
    Ok(())
}

/// An absent file is a legacy stopped instance. Publishing this marker does
/// not alter or copy its disk; an interrupted write leaves the original disk
/// and either the old absent state or the complete jailed state. The caller
/// holds the instance operation lock and has already proved the VM stopped.
pub(crate) fn ensure_jailed(inst: &Instance) -> Result<()> {
    let directory = PrivateDir::open_existing(&inst.dir)?;
    let name = OsStr::new(STATE_FILE);
    match directory.open_regular(name) {
        Ok(mut file) => {
            if file.metadata()?.len() > 1024 {
                bail!("Firecracker runtime state is too large");
            }
            let mut content = String::new();
            file.read_to_string(&mut content)
                .context("Cannot read Firecracker runtime state")?;
            parse_state(&content)
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            crate::fs_util::atomic_write_json(&inst.dir.join(STATE_FILE), JAILED_V1)
                .context("Cannot publish jailed Firecracker runtime state")
        }
        Err(error) => Err(error).context("Cannot inspect Firecracker runtime state"),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::config::{FirecrackerControlMode, ImageName, InstanceIndex, InstanceName};

    #[test]
    fn accepts_only_version_one_jailed_state() {
        assert!(parse_state(JAILED_V1).is_ok());
        assert!(parse_state(r#"{"version":1,"launcher":"direct_root"}"#).is_err());
        assert!(parse_state(r#"{"version":2,"launcher":"jailer"}"#).is_err());
        assert!(parse_state(r#"{"version":1,"launcher":"jailer","extra":0}"#).is_err());
    }

    #[test]
    fn stopped_legacy_migration_keeps_disk_and_rejects_unknown_versions() {
        let root = crate::test_support::private_tempdir().unwrap();
        let inst = Instance {
            name: InstanceName::new("migration-test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: root.path().to_path_buf(),
            image: ImageName::new("default").unwrap(),
            firecracker_control: FirecrackerControlMode::Vsock,
        };
        let disk = inst.rootfs_path();
        std::fs::write(&disk, b"original disk").unwrap();
        ensure_jailed(&inst).unwrap();
        assert_eq!(std::fs::read(&disk).unwrap(), b"original disk");
        assert_eq!(
            std::fs::read_to_string(root.path().join(STATE_FILE)).unwrap(),
            JAILED_V1
        );
        ensure_jailed(&inst).unwrap();
        let state_path = root.path().join(STATE_FILE);
        let padded = format!("{JAILED_V1}{}", " ".repeat(1024 - JAILED_V1.len()));
        std::fs::write(&state_path, &padded).unwrap();
        ensure_jailed(&inst).unwrap();
        std::fs::write(&state_path, format!("{padded} ")).unwrap();
        assert!(ensure_jailed(&inst).is_err());
        std::fs::write(&state_path, r#"{"version":1,"launcher":"direct_root"}"#).unwrap();
        assert!(ensure_jailed(&inst).is_err());
        std::fs::write(&state_path, r#"{"version":2,"launcher":"jailer"}"#).unwrap();
        assert!(ensure_jailed(&inst).is_err());
        assert_eq!(std::fs::read(&disk).unwrap(), b"original disk");
    }
}
