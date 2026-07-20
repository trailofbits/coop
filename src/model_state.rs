//! Per-instance local-vs-remote model selection.
//!
//! A coop VM routes its agents (Claude Code, Codex) at Anthropic's /
//! `OpenAI`'s cloud by default. `coop model <vm> local` flips the VM to a
//! host-side local model server (Ollama / LM Studio / vLLM / llama.cpp);
//! `coop model <vm> remote` flips it back. The choice is a per-VM
//! persisted setting rather than a per-launch flag, because coop cannot
//! inject env into a tool the user launches themselves inside a shell —
//! so the selection is materialized as guest config files that every
//! launch path (`coop shell`, `coop claude`, `coop codex`, VS Code)
//! reads.
//!
//! Persistence layout: one JSON file at `<inst.dir>/model.json`. The
//! default state (remote, no saved endpoints) is not written — a missing
//! file means "cloud defaults," matching the boot-time behaviour.
//!
//! Endpoint resolution per tool (see [`ModelState::resolved_claude`] /
//! [`ModelState::resolved_codex`]):
//!
//! 1. `[claude.local_model]` / `[codex.local_model]` in `config.toml`.
//! 2. Else the per-instance endpoint saved here (e.g. one prompted
//!    interactively when no config existed).
//! 3. Else none — the tool stays on the cloud default.
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{ClaudeConfig, CodexConfig, Instance, LocalModel};

/// Codex `model_provider` id written into `~/.codex/config.toml`.
pub const CODEX_LOCAL_PROVIDER: &str = "coop_local";

/// Env var Codex reads the local provider's API key from (`env_key`).
/// Forwarded to the guest with the configured (or dummy) token whenever
/// the VM is in local mode.
pub const CODEX_LOCAL_ENV_KEY: &str = "COOP_LOCAL_API_KEY";

/// Whether a VM's agents target the cloud or a host-side local server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelMode {
    /// Cloud defaults (Anthropic / `OpenAI`). The shipped default.
    #[default]
    Remote,
    /// A host-side local model server.
    Local,
}

impl ModelMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelMode::Remote => "remote",
            ModelMode::Local => "local",
        }
    }
}

/// Persisted per-instance model selection and any interactively-prompted
/// endpoints. Endpoints from `config.toml` take precedence over the saved
/// ones, so saving here only matters when `config.toml` has no entry.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ModelState {
    #[serde(default)]
    pub mode: ModelMode,

    /// Endpoint for Claude saved from an interactive prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_endpoint: Option<LocalModel>,

    /// Endpoint for Codex saved from an interactive prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_endpoint: Option<LocalModel>,

    /// Set once coop has materialized a Codex local-provider block in the
    /// guest. Unlike Claude's `settings.json` (rewritten unconditionally
    /// every boot), Codex's `config.toml` is only rewritten when coop has
    /// something to manage; this flag keeps that true after a switch to
    /// local, so a later switch to remote still rewrites a clean file even
    /// if the configured endpoint has since been removed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub codex_materialized: bool,
}

impl ModelState {
    /// `true` when nothing needs persisting: remote mode, no saved
    /// endpoints, and nothing materialized. Equivalent to "no `model.json`
    /// on disk."
    fn is_default(&self) -> bool {
        self.mode == ModelMode::Remote
            && self.claude_endpoint.is_none()
            && self.codex_endpoint.is_none()
            && !self.codex_materialized
    }

    pub fn save(&self, inst: &Instance) -> Result<()> {
        let path = inst.model_state_path();
        if self.is_default() {
            // A missing file already encodes the default; don't leave a
            // stale snapshot behind.
            if path.exists()
                && let Err(e) = fs::remove_file(&path)
            {
                tracing::debug!(
                    "Failed to remove default model state {} (non-fatal): {e}",
                    path.display()
                );
            }
            return Ok(());
        }
        let json = serde_json::to_string_pretty(self).context("Failed to serialize model state")?;
        // 0o600: an interactively-prompted `auth_token` is serialized here.
        crate::fs_util::atomic_write_with_mode(&path, &json, 0o600)
            .context("Failed to write model.json")?;
        tracing::debug!("Wrote model state to {}", path.display());
        Ok(())
    }

    pub fn try_load(inst: &Instance) -> Result<Option<Self>> {
        let path = inst.model_state_path();
        match fs::read_to_string(&path) {
            Ok(content) => {
                let state = serde_json::from_str(&content).context("Failed to parse model.json")?;
                Ok(Some(state))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(anyhow::Error::new(e).context(format!("Failed to read {}", path.display())))
            }
        }
    }

    /// Load the saved state, or the default (remote) when none exists.
    pub fn load_or_default(inst: &Instance) -> Result<Self> {
        Ok(Self::try_load(inst)?.unwrap_or_default())
    }

    /// The endpoint Claude should use, applying config-over-saved
    /// precedence. Independent of [`Self::mode`] — callers gate on the
    /// mode separately so they can show "would use" in status output.
    pub fn resolved_claude<'a>(&'a self, claude: &'a ClaudeConfig) -> Option<&'a LocalModel> {
        claude
            .local_model
            .as_ref()
            .or(self.claude_endpoint.as_ref())
    }

    /// The endpoint Codex should use, applying config-over-saved
    /// precedence. See [`Self::resolved_claude`].
    pub fn resolved_codex<'a>(&'a self, codex: &'a CodexConfig) -> Option<&'a LocalModel> {
        codex.local_model.as_ref().or(self.codex_endpoint.as_ref())
    }
}

/// The `env` block coop writes into the managed `~/.claude/settings.json`
/// to point Claude Code at a local endpoint.
///
/// `base_url` must already be rewritten to a guest-visible host (see
/// [`crate::network::rewrite_host_url`]). Every model tier is pinned to
/// the same local model so requests for any tier route locally. A
/// `BTreeMap` gives deterministic JSON for stable diffs and tests.
///
/// Two keys exist purely to keep a local inference server's KV cache
/// warm. Local backends (llama.cpp, vLLM) reuse the prompt cache only on
/// an exact prefix match, so anything Claude Code mutates per request
/// forces a full re-prefill of the 20K+ token system prompt — seconds of
/// latency on every tool call. We disable the two known mutators:
/// `CLAUDE_CODE_ATTRIBUTION_HEADER` (a per-request billing/version
/// fingerprint) and `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS` (the `git
/// status` snapshot, which changes every time a file is touched). Both
/// only matter against a self-hosted endpoint, and this block is written
/// only in local mode, so they are correctly scoped.
pub fn claude_env_block(base_url: &str, model: &str, auth_token: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("ANTHROPIC_BASE_URL".to_string(), base_url.to_string());
    env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), auth_token.to_string());
    env.insert(
        "CLAUDE_CODE_ATTRIBUTION_HEADER".to_string(),
        "0".to_string(),
    );
    env.insert(
        "CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS".to_string(),
        "1".to_string(),
    );
    for tier in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ] {
        env.insert(tier.to_string(), model.to_string());
    }
    env
}

/// The top-level keys coop merges into `~/.codex/config.toml` to route
/// Codex at a local endpoint: `model`, `model_provider`, and a
/// `[model_providers.coop_local]` block using the Responses API (the
/// only wire API current Codex supports).
///
/// `base_url` must already be rewritten to a guest-visible host.
pub fn codex_local_config(base_url: &str, model: &str) -> toml::Table {
    let mut provider = toml::Table::new();
    provider.insert(
        "name".to_string(),
        toml::Value::String("coop local model".to_string()),
    );
    provider.insert(
        "base_url".to_string(),
        toml::Value::String(base_url.to_string()),
    );
    provider.insert(
        "wire_api".to_string(),
        toml::Value::String("responses".to_string()),
    );
    provider.insert(
        "env_key".to_string(),
        toml::Value::String(CODEX_LOCAL_ENV_KEY.to_string()),
    );

    let mut providers = toml::Table::new();
    providers.insert(
        CODEX_LOCAL_PROVIDER.to_string(),
        toml::Value::Table(provider),
    );

    let mut root = toml::Table::new();
    root.insert("model".to_string(), toml::Value::String(model.to_string()));
    root.insert(
        "model_provider".to_string(),
        toml::Value::String(CODEX_LOCAL_PROVIDER.to_string()),
    );
    root.insert("model_providers".to_string(), toml::Value::Table(providers));
    root
}

/// The `~/.codex/config.toml` keys coop merges to route Codex through the
/// host-side injecting proxy (issue #411).
///
/// Mirrors [`codex_local_config`] but for proxy mode: it points the
/// `coop_local` provider's `base_url` at the proxy and selects it via
/// `model_provider`, while pinning **no** `model` — proxy mode is transparent,
/// so Codex keeps whatever model the user configured (or its default) and only
/// its egress is redirected. Codex sends the value of `env_key`
/// ([`CODEX_LOCAL_ENV_KEY`]) as `Authorization: Bearer`; coop forwards the
/// per-instance capability token there, which the proxy verifies before
/// injecting the real credential upstream.
pub fn codex_proxy_config(base_url: &str) -> toml::Table {
    let mut provider = toml::Table::new();
    provider.insert(
        "name".to_string(),
        toml::Value::String("coop credential proxy".to_string()),
    );
    provider.insert(
        "base_url".to_string(),
        toml::Value::String(base_url.to_string()),
    );
    provider.insert(
        "wire_api".to_string(),
        toml::Value::String("responses".to_string()),
    );
    provider.insert(
        "env_key".to_string(),
        toml::Value::String(CODEX_LOCAL_ENV_KEY.to_string()),
    );

    let mut providers = toml::Table::new();
    providers.insert(
        CODEX_LOCAL_PROVIDER.to_string(),
        toml::Value::Table(provider),
    );

    let mut root = toml::Table::new();
    root.insert(
        "model_provider".to_string(),
        toml::Value::String(CODEX_LOCAL_PROVIDER.to_string()),
    );
    root.insert("model_providers".to_string(), toml::Value::Table(providers));
    root
}

/// The `env` block coop writes into the managed `~/.claude/settings.json`
/// to point Claude Code at the host-side injecting proxy (issue #411).
///
/// Unlike [`claude_env_block`] (local mode), this pins **nothing** about the
/// model or request shape — proxy mode is transparent, routing real cloud
/// traffic through the host so the real credential can be injected upstream.
/// It sets only the base-URL override and the per-instance capability token
/// (`ANTHROPIC_AUTH_TOKEN`), which Claude Code sends as `Authorization:
/// Bearer` — the token the proxy verifies before injecting the real key.
pub fn claude_proxy_env_block(base_url: &str, capability_token: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("ANTHROPIC_BASE_URL".to_string(), base_url.to_string());
    env.insert(
        "ANTHROPIC_AUTH_TOKEN".to_string(),
        capability_token.to_string(),
    );
    env
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{
        ClaudeConfig, CodexConfig, ImageName, Instance, InstanceIndex, InstanceName, Secret,
    };

    fn fake_instance(dir: PathBuf) -> Instance {
        Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir,
            image: ImageName::new("test.img").unwrap(),
        }
    }

    fn endpoint(url: &str, model: &str, token: Option<&str>) -> LocalModel {
        LocalModel::new(
            url::Url::parse(url).unwrap(),
            model.to_string(),
            token.map(|t| Secret::new(t.to_string())),
        )
        .unwrap()
    }

    // ── ModelState persistence ───────────────────────────────

    #[test]
    fn save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        let state = ModelState {
            mode: ModelMode::Local,
            claude_endpoint: Some(endpoint("http://localhost:11434", "qwen", None)),
            codex_endpoint: None,
            codex_materialized: false,
        };
        state.save(&inst).unwrap();

        let loaded = ModelState::try_load(&inst).unwrap().unwrap();
        assert_eq!(loaded.mode, ModelMode::Local);
        assert_eq!(loaded.claude_endpoint.as_ref().unwrap().model(), "qwen");
        assert!(loaded.codex_endpoint.is_none());
    }

    #[test]
    fn mode_as_str_round_trips() {
        assert_eq!(ModelMode::Remote.as_str(), "remote");
        assert_eq!(ModelMode::Local.as_str(), "local");
    }

    #[test]
    fn try_load_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        assert!(ModelState::try_load(&inst).unwrap().is_none());
    }

    #[test]
    fn load_or_default_is_remote_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        assert_eq!(
            ModelState::load_or_default(&inst).unwrap().mode,
            ModelMode::Remote
        );
    }

    #[test]
    fn load_or_default_returns_saved_state() {
        // Distinguishes load_or_default from "always default": a saved
        // non-default (Local) state must come back as Local, not Remote.
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        ModelState {
            mode: ModelMode::Local,
            claude_endpoint: Some(endpoint("http://localhost:11434", "qwen", None)),
            ..Default::default()
        }
        .save(&inst)
        .unwrap();
        let loaded = ModelState::load_or_default(&inst).unwrap();
        assert_eq!(loaded.mode, ModelMode::Local);
        assert_eq!(loaded.claude_endpoint.as_ref().unwrap().model(), "qwen");
    }

    #[test]
    fn default_state_is_not_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        ModelState::default().save(&inst).unwrap();
        assert!(!inst.model_state_path().exists());
    }

    #[test]
    fn save_default_removes_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        ModelState {
            mode: ModelMode::Local,
            ..Default::default()
        }
        .save(&inst)
        .unwrap();
        assert!(inst.model_state_path().exists());

        ModelState::default().save(&inst).unwrap();
        assert!(!inst.model_state_path().exists());
    }

    #[test]
    fn remote_with_saved_endpoint_is_persisted() {
        // `coop model remote` must keep the prompted endpoint so the next
        // `local` doesn't re-prompt.
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        ModelState {
            mode: ModelMode::Remote,
            claude_endpoint: Some(endpoint("http://localhost:1234", "m", None)),
            codex_endpoint: None,
            codex_materialized: false,
        }
        .save(&inst)
        .unwrap();
        let loaded = ModelState::try_load(&inst).unwrap().unwrap();
        assert_eq!(loaded.mode, ModelMode::Remote);
        assert!(loaded.claude_endpoint.is_some());
    }

    #[test]
    fn remote_with_codex_materialized_is_persisted() {
        // The materialized flag must survive a switch back to remote so the
        // next boot still rewrites a clean Codex config.toml even when the
        // configured endpoint has since been removed.
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        ModelState {
            mode: ModelMode::Remote,
            claude_endpoint: None,
            codex_endpoint: None,
            codex_materialized: true,
        }
        .save(&inst)
        .unwrap();
        assert!(inst.model_state_path().exists());
        assert!(
            ModelState::try_load(&inst)
                .unwrap()
                .unwrap()
                .codex_materialized
        );
    }

    #[test]
    fn try_load_surfaces_non_not_found_read_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        fs::create_dir_all(inst.model_state_path()).unwrap();
        assert!(ModelState::try_load(&inst).is_err());
    }

    // ── endpoint resolution ──────────────────────────────────

    #[test]
    fn config_endpoint_takes_precedence_over_saved() {
        let claude = ClaudeConfig {
            local_model: Some(endpoint("http://localhost:1/", "from-config", None)),
            ..Default::default()
        };
        let state = ModelState {
            mode: ModelMode::Local,
            claude_endpoint: Some(endpoint("http://localhost:2/", "from-saved", None)),
            codex_endpoint: None,
            codex_materialized: false,
        };
        assert_eq!(
            state.resolved_claude(&claude).unwrap().model(),
            "from-config"
        );
    }

    #[test]
    fn saved_endpoint_used_when_config_absent() {
        let claude = ClaudeConfig::default();
        let state = ModelState {
            mode: ModelMode::Local,
            claude_endpoint: Some(endpoint("http://localhost:2/", "from-saved", None)),
            codex_endpoint: None,
            codex_materialized: false,
        };
        assert_eq!(
            state.resolved_claude(&claude).unwrap().model(),
            "from-saved"
        );
    }

    #[test]
    fn resolves_none_when_neither_config_nor_saved() {
        let codex = CodexConfig::default();
        let state = ModelState::default();
        assert!(state.resolved_codex(&codex).is_none());
    }

    // ── env / provider materialization ───────────────────────

    #[test]
    fn claude_env_block_pins_every_tier_to_local_model() {
        let env = claude_env_block("http://172.16.0.1:11434", "qwen2.5-coder", "tok");
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://172.16.0.1:11434");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "tok");
        for tier in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        ] {
            assert_eq!(env[tier], "qwen2.5-coder", "tier {tier} not pinned");
        }
    }

    #[test]
    fn claude_proxy_env_block_sets_only_base_url_and_token() {
        // Proxy mode is transparent: it must NOT pin any model tier or touch
        // cache-buster keys (those are local-mode concerns) — only the base
        // URL and the capability token.
        let env = claude_proxy_env_block("http://172.16.0.1:8788", "cap-token");
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://172.16.0.1:8788");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "cap-token");
        assert_eq!(
            env.len(),
            2,
            "proxy env block must set nothing else: {env:?}"
        );
    }

    #[test]
    fn claude_env_block_disables_prefix_cache_busters() {
        // Both keys keep a local KV cache warm by stopping Claude Code
        // from mutating the system prompt per request (see the doc on
        // `claude_env_block`). A regression here silently restores the
        // ~60s-per-tool-call latency on local backends.
        let env = claude_env_block("http://172.16.0.1:11434", "qwen2.5-coder", "tok");
        assert_eq!(env["CLAUDE_CODE_ATTRIBUTION_HEADER"], "0");
        assert_eq!(env["CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS"], "1");
    }

    #[test]
    fn codex_local_config_uses_responses_wire_api() {
        let table = codex_local_config("http://host.lima.internal:11434/v1/", "gpt-oss:120b");
        assert_eq!(table["model"].as_str().unwrap(), "gpt-oss:120b");
        assert_eq!(
            table["model_provider"].as_str().unwrap(),
            CODEX_LOCAL_PROVIDER
        );
        let provider = table["model_providers"][CODEX_LOCAL_PROVIDER]
            .as_table()
            .unwrap();
        assert_eq!(
            provider["base_url"].as_str().unwrap(),
            "http://host.lima.internal:11434/v1/"
        );
        assert_eq!(provider["wire_api"].as_str().unwrap(), "responses");
        assert_eq!(provider["env_key"].as_str().unwrap(), CODEX_LOCAL_ENV_KEY);
    }

    #[test]
    fn codex_proxy_config_pins_provider_but_not_model() {
        let table = codex_proxy_config("http://127.0.0.1:8900");
        // Proxy mode is transparent: no model pin, so Codex keeps its own.
        assert!(
            !table.contains_key("model"),
            "proxy mode must not pin a model"
        );
        assert_eq!(
            table["model_provider"].as_str().unwrap(),
            CODEX_LOCAL_PROVIDER
        );
        let provider = table["model_providers"][CODEX_LOCAL_PROVIDER]
            .as_table()
            .unwrap();
        assert_eq!(
            provider["base_url"].as_str().unwrap(),
            "http://127.0.0.1:8900"
        );
        assert_eq!(provider["wire_api"].as_str().unwrap(), "responses");
        assert_eq!(provider["env_key"].as_str().unwrap(), CODEX_LOCAL_ENV_KEY);
        assert_eq!(provider["name"].as_str().unwrap(), "coop credential proxy");
    }
}
