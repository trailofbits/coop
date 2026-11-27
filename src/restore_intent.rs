//! Durable gate between a VM disk replacement and its published image contract.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{ImageName, Instance};
use crate::setup::TemplateConfig;

thread_local! {
    static PROVISIONING_PERMITS: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestorePhase {
    DiskSwapPending,
    ProvisioningPending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestoreIntent {
    pub image: ImageName,
    pub template: TemplateConfig,
    pub phase: RestorePhase,
}

impl RestoreIntent {
    fn path(inst: &Instance) -> PathBuf {
        inst.dir.join("restore-intent.json")
    }

    pub(crate) fn load(inst: &Instance) -> Result<Option<Self>> {
        let path = Self::path(inst);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", path.display()));
            }
        };
        Ok(Some(serde_json::from_str(&content).with_context(|| {
            format!("Failed to parse {}", path.display())
        })?))
    }

    pub(crate) fn save(&self, inst: &Instance) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        crate::fs_util::atomic_write_json(&Self::path(inst), &json)
    }

    pub(crate) fn clear(inst: &Instance) -> Result<()> {
        match fs::remove_file(Self::path(inst)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("Failed to clear restore intent"),
        }
    }
}

pub(crate) fn ensure_contract_ready(inst: &Instance) -> Result<()> {
    let Some(intent) = RestoreIntent::load(inst)? else {
        return Ok(());
    };
    let permitted =
        PROVISIONING_PERMITS.with(|permits| permits.borrow().contains(&RestoreIntent::path(inst)));
    if permitted && intent.phase == RestorePhase::ProvisioningPending {
        return Ok(());
    }
    bail!(
        "VM '{}' has an incomplete restore to image '{}' ({}); retry `coop restore {} --image {} --reprovision` before starting or using its guest",
        inst.name,
        intent.image,
        match intent.phase {
            RestorePhase::DiskSwapPending => "disk swap pending",
            RestorePhase::ProvisioningPending => "provisioning pending",
        },
        inst.name,
        intent.image,
    )
}

pub(crate) fn with_provisioning_permit<T>(
    inst: &Instance,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    struct Permit(PathBuf);
    impl Drop for Permit {
        fn drop(&mut self) {
            PROVISIONING_PERMITS.with(|permits| {
                let mut permits = permits.borrow_mut();
                if let Some(index) = permits.iter().rposition(|path| path == &self.0) {
                    permits.remove(index);
                }
            });
        }
    }
    let path = RestoreIntent::path(inst);
    PROVISIONING_PERMITS.with(|permits| permits.borrow_mut().push(path.clone()));
    let _permit = Permit(path);
    work()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixtures and assertions")]
mod tests {
    use super::{RestoreIntent, RestorePhase, ensure_contract_ready, with_provisioning_permit};
    use crate::config::{FirecrackerControlMode, ImageName, Instance, InstanceIndex, InstanceName};
    use crate::setup::{TEMPLATE_VERSION, TemplateConfig};

    #[test]
    fn pending_restore_blocks_guest_until_provisioning_is_complete() {
        let root = crate::test_support::private_tempdir().unwrap();
        let inst = Instance {
            name: InstanceName::new("pending").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: root.path().to_path_buf(),
            image: ImageName::new("old").unwrap(),
            firecracker_control: FirecrackerControlMode::Vsock,
        };
        inst.save().unwrap();
        let target = ImageName::new("codex-only").unwrap();
        let template = TemplateConfig {
            version: TEMPLATE_VERSION,
            agents: crate::agents::AgentSet::new(vec![crate::agents::AgentId::Codex]).unwrap(),
            created: "2026-01-01T00:00:00Z".into(),
            install_script_hash: crate::sha256_hash::Sha256Hash::of("fixture"),
            profiles: Vec::new(),
            extra_packages: Vec::new(),
            post_install_hash: None,
            marketplaces: Vec::new(),
            plugins: Vec::new(),
            codex_marketplaces: Vec::new(),
            codex_plugins: Vec::new(),
            grok_marketplaces: Vec::new(),
            grok_plugins: Vec::new(),
            guest_user: crate::guest::GuestUser::default(),
            capabilities: std::collections::BTreeSet::default(),
        };
        let mut intent = RestoreIntent {
            image: target,
            template,
            phase: RestorePhase::DiskSwapPending,
        };
        intent.save(&inst).unwrap();
        assert!(ensure_contract_ready(&inst).is_err());
        assert!(with_provisioning_permit(&inst, || ensure_contract_ready(&inst)).is_err());
        intent.phase = RestorePhase::ProvisioningPending;
        intent.save(&inst).unwrap();
        with_provisioning_permit(&inst, || ensure_contract_ready(&inst)).unwrap();
        assert!(ensure_contract_ready(&inst).is_err());
        RestoreIntent::clear(&inst).unwrap();
        ensure_contract_ready(&inst).unwrap();
    }
}
