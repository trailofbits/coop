//! Per-instance credential-proxy overrides (issue #411).
//!
//! The `[proxy.<provider>]` blocks in `config.toml` are the **defaults** that
//! apply to every VM. A single VM can override the credential for a provider
//! — for per-project billing, scope, or revocation — without a growing config
//! table: the override is persisted here, one JSON file at
//! `<inst.dir>/proxy.json`, mirroring [`crate::model_state`].
//!
//! Resolution per provider is **override → default → off**: a per-VM override
//! wins over the config default, and if neither is set the proxy is off for
//! that provider. Whichever wins carries a `cmd:` credential reference that is
//! resolved on the host at VM start (never written to the guest); a
//! configured-but-unresolvable credential fails the VM start closed rather
//! than falling back to forwarding a raw key.
use std::fs;
use std::io::ErrorKind;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Instance, ProxyConfig, ProxyUpstream};
use crate::proxy::Provider;

/// Persisted per-instance credential overrides. Absent fields (the common
/// case) fall back to the `[proxy.<provider>]` config defaults.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProxyState {
    /// Overrides the `[proxy.anthropic]` default for this VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<ProxyUpstream>,

    /// Overrides the `[proxy.openai]` default for this VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai: Option<ProxyUpstream>,
}

impl ProxyState {
    /// The override slot for `provider`, mutable — used by `coop proxy setup
    /// --vm` to record a per-VM credential.
    pub fn slot_mut(&mut self, provider: Provider) -> &mut Option<ProxyUpstream> {
        match provider {
            Provider::Anthropic => &mut self.anthropic,
            Provider::Openai => &mut self.openai,
        }
    }

    /// The per-VM override for `provider`, if any.
    fn override_for(&self, provider: Provider) -> Option<&ProxyUpstream> {
        match provider {
            Provider::Anthropic => self.anthropic.as_ref(),
            Provider::Openai => self.openai.as_ref(),
        }
    }

    /// The effective upstream for `provider`: the per-VM override if set,
    /// otherwise the config default, otherwise `None` (proxy off).
    pub fn effective<'a>(
        &'a self,
        provider: Provider,
        cfg: &'a ProxyConfig,
        default_for: fn(&ProxyConfig) -> Option<&ProxyUpstream>,
    ) -> Option<&'a ProxyUpstream> {
        self.override_for(provider).or_else(|| default_for(cfg))
    }

    /// `true` when nothing needs persisting — equivalent to "no `proxy.json`
    /// on disk."
    fn is_default(&self) -> bool {
        self.anthropic.is_none() && self.openai.is_none()
    }

    pub fn save(&self, inst: &Instance) -> Result<()> {
        let path = inst.proxy_state_path();
        if self.is_default() {
            // A missing file already encodes "no override"; don't leave a
            // stale snapshot behind.
            if path.exists()
                && let Err(e) = fs::remove_file(&path)
            {
                tracing::debug!(
                    "Failed to remove default proxy state {} (non-fatal): {e}",
                    path.display()
                );
            }
            return Ok(());
        }
        let json = serde_json::to_string_pretty(self).context("Failed to serialize proxy state")?;
        // 0o600: an override may carry a literal (BYO) credential, and even a
        // `cmd:` reference is per-VM state we keep owner-only for consistency
        // with model.json.
        crate::fs_util::atomic_write_with_mode(&path, &json, 0o600)
            .context("Failed to write proxy.json")?;
        tracing::debug!("Wrote proxy state to {}", path.display());
        Ok(())
    }

    pub fn try_load(inst: &Instance) -> Result<Option<Self>> {
        let path = inst.proxy_state_path();
        match fs::read_to_string(&path) {
            Ok(content) => {
                let state = serde_json::from_str(&content).context("Failed to parse proxy.json")?;
                Ok(Some(state))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(anyhow::Error::new(e).context(format!("Failed to read {}", path.display())))
            }
        }
    }

    /// Load the saved overrides, or the empty default when none exist.
    pub fn load_or_default(inst: &Instance) -> Result<Self> {
        Ok(Self::try_load(inst)?.unwrap_or_default())
    }
}

/// The config default accessor for a provider — pairs with
/// [`ProxyState::effective`] so callers name the provider once.
pub fn config_default(provider: Provider) -> fn(&ProxyConfig) -> Option<&ProxyUpstream> {
    match provider {
        Provider::Anthropic => |cfg| cfg.anthropic.as_ref(),
        Provider::Openai => |cfg| cfg.openai.as_ref(),
    }
}

/// The effective upstream for `provider` on `inst`: per-VM override → config
/// default → `None`. A convenience over [`ProxyState::effective`] that loads
/// the persisted state.
pub fn effective_upstream(
    inst: &Instance,
    provider: Provider,
    cfg: &ProxyConfig,
) -> Result<Option<ProxyUpstream>> {
    let state = ProxyState::load_or_default(inst)?;
    Ok(state
        .effective(provider, cfg, config_default(provider))
        .cloned())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::config::{ImageName, InstanceIndex, InstanceName, ProxyAuthScheme, Secret};
    use std::path::PathBuf;

    fn inst(dir: PathBuf) -> Instance {
        Instance {
            name: InstanceName::new("t").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir,
            image: ImageName::new("t.img").unwrap(),
        }
    }

    fn upstream(cred: &str, auth: ProxyAuthScheme) -> ProxyUpstream {
        ProxyUpstream {
            credential: Secret::new(cred.to_string()),
            auth,
        }
    }

    #[test]
    fn override_wins_over_default() {
        let cfg = ProxyConfig {
            anthropic: Some(upstream("cmd:default", ProxyAuthScheme::Bearer)),
            ..Default::default()
        };
        let state = ProxyState {
            anthropic: Some(upstream("cmd:override", ProxyAuthScheme::ApiKey)),
            ..Default::default()
        };

        let eff = state
            .effective(
                Provider::Anthropic,
                &cfg,
                config_default(Provider::Anthropic),
            )
            .unwrap();
        assert_eq!(eff.credential.expose(), "cmd:override");
        assert_eq!(eff.auth, ProxyAuthScheme::ApiKey);
    }

    #[test]
    fn falls_back_to_default_then_off() {
        let cfg = ProxyConfig {
            openai: Some(upstream("cmd:default-oai", ProxyAuthScheme::Bearer)),
            ..Default::default()
        };
        let state = ProxyState::default();

        let openai = state
            .effective(Provider::Openai, &cfg, config_default(Provider::Openai))
            .unwrap();
        assert_eq!(openai.credential.expose(), "cmd:default-oai");
        // No default and no override → off.
        assert!(
            state
                .effective(
                    Provider::Anthropic,
                    &cfg,
                    config_default(Provider::Anthropic)
                )
                .is_none()
        );
    }

    #[test]
    fn save_load_round_trip_and_default_removes_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let inst = inst(tmp.path().to_path_buf());

        let state = ProxyState {
            openai: Some(upstream("cmd:x", ProxyAuthScheme::Bearer)),
            ..Default::default()
        };
        state.save(&inst).unwrap();
        assert!(inst.proxy_state_path().exists());

        let loaded = ProxyState::load_or_default(&inst).unwrap();
        assert_eq!(loaded.openai.unwrap().credential.expose(), "cmd:x");

        // Clearing every override removes the file.
        ProxyState::default().save(&inst).unwrap();
        assert!(!inst.proxy_state_path().exists());
    }

    #[test]
    fn load_or_default_is_empty_when_file_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let inst = inst(tmp.path().to_path_buf());
        assert!(!inst.proxy_state_path().exists());

        let loaded = ProxyState::load_or_default(&inst).unwrap();
        assert!(loaded.anthropic.is_none() && loaded.openai.is_none());
        // The missing-file → None path also drives effective resolution off.
        let cfg = ProxyConfig::default();
        assert!(
            effective_upstream(&inst, Provider::Openai, &cfg)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn effective_upstream_loads_from_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let inst = inst(tmp.path().to_path_buf());
        let state = ProxyState {
            anthropic: Some(upstream("cmd:vm", ProxyAuthScheme::Bearer)),
            ..Default::default()
        };
        state.save(&inst).unwrap();

        let cfg = ProxyConfig::default();
        let eff = effective_upstream(&inst, Provider::Anthropic, &cfg)
            .unwrap()
            .unwrap();
        assert_eq!(eff.credential.expose(), "cmd:vm");
    }
}
