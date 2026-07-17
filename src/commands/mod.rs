//! Command implementations for the `coop` CLI.
//!
//! Each `cmd_*` entry point lives in a domain submodule; this module
//! re-exports the `pub(crate)` dispatch surface [`crate::run`] consumes
//! and holds the cross-domain orchestration helpers the submodules share.

mod admin;
mod agent;
mod devcontainer;
mod github;
pub(crate) mod json;
mod lifecycle;
mod model;
mod profiles;
mod quickstart;

pub(crate) use admin::{UninstallOpts, cmd_init, cmd_uninstall, cmd_validate};
pub(crate) use agent::{AgentSelection, AgentUpdateOpts, cmd_agent_update};
pub(crate) use devcontainer::{
    DevcontainerInput, DevcontainerOpts, cmd_devcontainer, cmd_devcontainer_check,
    resolve_devcontainer, resolve_devcontainer_collect,
};
pub(crate) use github::cmd_github;
pub(crate) use lifecycle::{
    ProfileImageTarget, ProjectTransport, ResizeOpts, StartOpts, UpDevcontainerOpts, UpOpts,
    UpRuntimeOpts, apply_runtime_guest_env, apply_vm_overrides, cmd_commit, cmd_destroy, cmd_exec,
    cmd_list, cmd_resize, cmd_restore, cmd_shell, cmd_start, cmd_status, cmd_stop, cmd_up,
    codex_launch_args, open_ssh_session, preflight_start_target, prepare_session_from_target,
    prepend_binary, resolve_running,
};
pub(crate) use model::cmd_model;
pub(crate) use profiles::{cmd_images, cmd_profiles};
pub(crate) use quickstart::{QuickstartOpts, cmd_quickstart};

use anyhow::Result;

use crate::backend::VmBackend as _;
use crate::cmd::Cmd;
use crate::{backend, config, guest_env_state, port_forward, workspace};

/// Merge guest-env entries by precedence (CLI > devcontainer.json > config.toml),
/// persist the result into `cfg.guest_env`, and return the merged map.
///
/// This is the single implementation of the precedence rule. All call sites —
/// the `start_opts`-mutating [`apply_runtime_guest_env`] and the struct-literal
/// construction paths — route through here.
fn merge_runtime_guest_env(
    cfg: &mut config::CoopConfig,
    cli_guest_env: &[(guest_env_state::EnvVarName, String)],
    dc_guest_env: Option<&std::collections::BTreeMap<guest_env_state::EnvVarName, String>>,
) -> std::collections::BTreeMap<guest_env_state::EnvVarName, String> {
    let cli_guest_env: std::collections::BTreeMap<_, _> = cli_guest_env.iter().cloned().collect();
    let dc_guest_env = dc_guest_env.cloned().unwrap_or_default();
    let persisted_guest_env =
        guest_env_state::merge_persisted_entries(&dc_guest_env, &cli_guest_env);
    for (key, value) in &persisted_guest_env {
        cfg.guest_env.insert(key.clone(), value.clone());
    }
    persisted_guest_env
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
#[expect(clippy::expect_used, reason = "test code — panics are assertions")]
mod tests {
    use proptest::prelude::*;

    /// Guest-env maps keyed in a deliberately small space so the three tiers
    /// overlap often, exercising the precedence path rather than only disjoint
    /// unions.
    fn small_env_map()
    -> impl Strategy<Value = std::collections::BTreeMap<super::guest_env_state::EnvVarName, String>>
    {
        prop::collection::btree_map(
            "[A-E]".prop_map(|s| {
                super::guest_env_state::EnvVarName::new(&s)
                    .expect("single uppercase letter is valid")
            }),
            any::<String>(),
            0..6,
        )
    }

    #[test]
    fn merge_runtime_guest_env_applies_three_tier_precedence() {
        use super::guest_env_state::EnvVarName;

        let env = |s: &str| EnvVarName::new(s).expect("valid env var");

        let mut cfg = super::config::CoopConfig::default();
        cfg.guest_env.insert(env("ONLY_CFG"), "cfg".to_string());
        cfg.guest_env.insert(env("SHARED"), "cfg".to_string());

        let mut dc = std::collections::BTreeMap::new();
        dc.insert(env("ONLY_DC"), "dc".to_string());
        dc.insert(env("SHARED"), "dc".to_string());

        let cli = vec![
            (env("ONLY_CLI"), "cli".to_string()),
            (env("SHARED"), "cli".to_string()),
        ];

        let merged = super::merge_runtime_guest_env(&mut cfg, &cli, Some(&dc));

        // CLI wins over devcontainer wins over config.toml on conflict.
        assert_eq!(merged.get(&env("SHARED")).map(String::as_str), Some("cli"));
        assert_eq!(
            cfg.guest_env.get(&env("SHARED")).map(String::as_str),
            Some("cli")
        );

        // The merged map carries dc + cli entries; config-only entries are not
        // re-added to it but remain in cfg.guest_env.
        assert_eq!(merged.get(&env("ONLY_DC")).map(String::as_str), Some("dc"));
        assert_eq!(
            merged.get(&env("ONLY_CLI")).map(String::as_str),
            Some("cli")
        );
        assert!(!merged.contains_key(&env("ONLY_CFG")));

        // The side effect folds dc + cli into cfg.guest_env without dropping
        // the pre-existing config.toml-only entry.
        assert_eq!(
            cfg.guest_env.get(&env("ONLY_CFG")).map(String::as_str),
            Some("cfg")
        );
        assert_eq!(
            cfg.guest_env.get(&env("ONLY_DC")).map(String::as_str),
            Some("dc")
        );
        assert_eq!(
            cfg.guest_env.get(&env("ONLY_CLI")).map(String::as_str),
            Some("cli")
        );
    }

    #[test]
    fn merge_runtime_guest_env_without_devcontainer_uses_cli_only() {
        use super::guest_env_state::EnvVarName;

        let env = |s: &str| EnvVarName::new(s).expect("valid env var");

        let mut cfg = super::config::CoopConfig::default();
        cfg.guest_env.insert(env("ONLY_CFG"), "cfg".to_string());

        let cli = vec![(env("FROM_CLI"), "cli".to_string())];
        let merged = super::merge_runtime_guest_env(&mut cfg, &cli, None);

        assert_eq!(merged, cli.into_iter().collect());
        assert_eq!(
            cfg.guest_env.get(&env("FROM_CLI")).map(String::as_str),
            Some("cli")
        );
        assert_eq!(
            cfg.guest_env.get(&env("ONLY_CFG")).map(String::as_str),
            Some("cfg")
        );
    }

    proptest! {
        /// `merge_runtime_guest_env` resolves all three tiers with
        /// CLI > devcontainer.json > config.toml precedence. The returned map
        /// carries exactly the devcontainer ∪ CLI entries (config-only keys
        /// stay in `cfg.guest_env` but are never re-added to the persisted
        /// map), and `cfg.guest_env` reflects the full three-tier merge.
        #[test]
        fn merge_runtime_guest_env_precedence_holds(
            cfg_env in small_env_map(),
            dc in small_env_map(),
            cli_map in small_env_map(),
        ) {
            use super::guest_env_state::EnvVarName;

            let mut cfg = super::config::CoopConfig::default();
            for (k, v) in &cfg_env {
                cfg.guest_env.insert(k.clone(), v.clone());
            }
            let cli: Vec<(EnvVarName, String)> =
                cli_map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

            let merged = super::merge_runtime_guest_env(&mut cfg, &cli, Some(&dc));

            // Returned map = devcontainer ∪ CLI (CLI wins); no config-only keys.
            let persisted_union: std::collections::BTreeSet<_> =
                dc.keys().chain(cli_map.keys()).cloned().collect();
            let got: std::collections::BTreeSet<_> = merged.keys().cloned().collect();
            prop_assert_eq!(got, persisted_union);
            for (k, v) in &merged {
                let expected = cli_map.get(k).or_else(|| dc.get(k)).expect("key from a tier");
                prop_assert_eq!(v, expected);
            }

            // cfg.guest_env now reflects the full three-tier precedence over
            // every key seen in any tier.
            let all_keys: std::collections::BTreeSet<_> = cfg_env
                .keys()
                .chain(dc.keys())
                .chain(cli_map.keys())
                .cloned()
                .collect();
            for k in &all_keys {
                let expected = cli_map
                    .get(k)
                    .or_else(|| dc.get(k))
                    .or_else(|| cfg_env.get(k))
                    .expect("key from a tier");
                prop_assert_eq!(cfg.guest_env.get(k).expect("merged key present"), expected);
            }
        }
    }
}
