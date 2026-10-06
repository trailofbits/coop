//! Command implementations for the `coop` CLI.
//!
//! Each `cmd_*` entry point lives in a domain submodule; this module
//! re-exports the `pub(crate)` dispatch surface [`crate::run`] consumes
//! and holds the cross-domain orchestration helpers the submodules share.

mod admin;
mod agent;
mod github;
pub(crate) mod json;
mod lifecycle;
mod model;
mod profiles;
mod proxy;
mod quickstart;

pub(crate) use admin::{UninstallOpts, cmd_init, cmd_uninstall, cmd_validate};
pub(crate) use agent::{AgentSelection, AgentUpdateOpts, cmd_agent_update};
pub(crate) use github::cmd_github;
pub(crate) use lifecycle::{
    ProfileImageTarget, ProjectTransport, ReprovisionOpts, ResizeOpts, RestoreMode, RestoreOpts,
    StartOpts, UpOpts, UpRuntimeOpts, apply_runtime_guest_env, apply_vm_overrides, cmd_commit,
    cmd_destroy, cmd_exec, cmd_list, cmd_resize, cmd_restore, cmd_shell, cmd_start, cmd_status,
    cmd_stop, cmd_up, codex_launch_args, grok_launch_args, open_ssh_session,
    preflight_start_target, prepare_session_from_target, prepend_binary, resolve_running,
};
pub(crate) use model::cmd_model;
pub(crate) use profiles::{cmd_images, cmd_profiles};
pub(crate) use proxy::cmd_proxy;
pub(crate) use quickstart::{QuickstartOpts, cmd_quickstart};

use std::collections::BTreeMap;

use anyhow::Result;

use crate::backend::VmBackend as _;
use crate::cmd::Cmd;
use crate::{backend, config, guest_env_state, port_forward, workspace};

/// Persist CLI guest-env entries into config and instance state.
fn merge_runtime_guest_env(
    cfg: &mut config::CoopConfig,
    cli_guest_env: &[(guest_env_state::EnvVarName, String)],
) -> BTreeMap<guest_env_state::EnvVarName, String> {
    let entries: BTreeMap<_, _> = cli_guest_env.iter().cloned().collect();
    for (key, value) in &entries {
        cfg.guest_env.insert(key.clone(), value.clone());
    }
    entries
}

/// Destroy every instance, delegate backend-specific shared-state cleanup
/// (`destroy_shared`), wipe the SSH keypair and instances dir, and strip every
/// coop block from `~/.ssh/config`.
///
/// Shared by `coop destroy --all` and `coop uninstall`. Does **not** remove
/// the `data_dir` itself or the binary — uninstall handles those.
fn purge_all_data(be: &backend::PlatformBackend, cfg: &config::CoopConfig) -> Result<()> {
    let instances = cfg.list_instances()?;
    for inst in &instances {
        tracing::info!("Destroying instance '{}'", inst.name);
        if let Ok(target) = be.ssh_target(cfg, inst) {
            port_forward::teardown_ssh_forwards(inst, &target);
        }
        crate::proxy::stop(inst);
        be.destroy_instance(cfg, inst)?;
        workspace::remove_ssh_config(inst)?;
    }

    be.destroy_shared(cfg);

    let key = cfg.ssh_key_path();
    if let Err(e) = std::fs::remove_file(&key) {
        tracing::debug!("Failed to remove SSH private key (non-fatal): {e}");
    }
    if let Err(e) = std::fs::remove_file(key.with_extension("pub")) {
        tracing::debug!("Failed to remove SSH public key (non-fatal): {e}");
    }

    let instances_dir = cfg.instances_dir();
    if instances_dir.exists() {
        // Instance dirs may be root-owned (Firecracker) or user-owned (Lima)
        if let Err(e) = std::fs::remove_dir_all(&instances_dir) {
            tracing::debug!("User remove_dir_all failed, trying sudo: {e}");
            if let Err(e) = Cmd::new("rm").arg("-rf").arg(&instances_dir).sudo().run() {
                tracing::debug!(
                    "Failed to remove instances dir {} (non-fatal): {e}",
                    instances_dir.display()
                );
            }
        }
    }

    workspace::remove_all_ssh_config()?;
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {
    #[test]
    fn cli_guest_env_overrides_config_and_persists_only_cli_entries() {
        let key = crate::guest_env_state::EnvVarName::new("K").unwrap();
        let only_config = crate::guest_env_state::EnvVarName::new("CONFIG_ONLY").unwrap();
        let mut cfg = crate::config::CoopConfig::default();
        cfg.guest_env.insert(key.clone(), "config".into());
        cfg.guest_env.insert(only_config.clone(), "keep".into());
        let persisted = super::merge_runtime_guest_env(&mut cfg, &[(key.clone(), "cli".into())]);
        assert_eq!(cfg.guest_env.get(&key).map(String::as_str), Some("cli"));
        assert_eq!(
            cfg.guest_env.get(&only_config).map(String::as_str),
            Some("keep")
        );
        assert_eq!(persisted.get(&key).map(String::as_str), Some("cli"));
        assert!(!persisted.contains_key(&only_config));
    }
}
