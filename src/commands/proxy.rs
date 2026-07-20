//! `coop proxy` subcommands (issue #411).
//!
//! `coop proxy setup` mirrors the GitHub PAT wizard: take a provider
//! credential pasted at the prompt, store it in a secret backend of the user's
//! choice, and write the resulting `cmd:` reference into `[proxy.<provider>]`
//! (the global default) or into a per-VM override — so the credential is never
//! a plaintext value in the config, and (in proxy mode) never enters the
//! guest. `coop proxy status` shows what each VM's agents resolve to, with
//! credentials redacted.

#![expect(
    clippy::print_stderr,
    reason = "setup wizard is interactive CLI — stderr is user communication"
)]

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::ProxyAction;
use crate::config::{
    CoopConfig, InstanceName, ProxyAuthScheme, ProxyConfig, ProxyUpstream, Secret,
};
use crate::github_pat::{pick_backend, read_token_no_echo};
use crate::proxy::Provider;
use crate::proxy_state::ProxyState;
use crate::secret_store::{self, ANTHROPIC_SERVICE, AccountName, OPENAI_SERVICE};

pub(crate) fn cmd_proxy(cfg: &CoopConfig, config_path: &Path, action: &ProxyAction) -> Result<()> {
    match action {
        ProxyAction::Setup {
            openai,
            anthropic: _,
            vm,
            api_key,
        } => {
            let provider = if *openai {
                Provider::Openai
            } else {
                Provider::Anthropic
            };
            run_setup(cfg, config_path, provider, vm.as_deref(), *api_key)
        }
        ProxyAction::Status { vm } => run_status(cfg, vm.as_deref()),
    }
}

// ── setup ────────────────────────────────────────────────────

fn run_setup(
    cfg: &CoopConfig,
    config_path: &Path,
    provider: Provider,
    vm: Option<&str>,
    api_key: bool,
) -> Result<()> {
    // Validate the target VM (name + existence) up front, before the raw `--vm`
    // string reaches the secret-store filesystem path or a secret is written:
    // an invalid name must not become a path segment, and an unknown VM must
    // not leave an orphaned secret behind (parse-don't-validate at the boundary).
    let inst = match vm {
        Some(vm) => {
            let name = InstanceName::new(vm)
                .with_context(|| format!("'{vm}' is not a valid instance name"))?;
            Some(cfg.resolve_instance(Some(&name))?)
        }
        None => None,
    };

    let auth = auth_for(provider, api_key);
    print_setup_guidance(provider, auth);

    let token = read_token_no_echo()?;
    let backend = pick_backend()?;

    let service = service_for(provider, inst.as_ref().map(|i| i.name.as_str()));
    let account =
        AccountName::new(provider.name()).context("internal error: invalid proxy account name")?;
    let state_dir = cfg.data_dir.join("state");
    let cmd_token = secret_store::store_secret(backend, &service, &account, &token, &state_dir)
        .with_context(|| {
            format!(
                "Failed to store the {} credential in the chosen backend",
                provider.name()
            )
        })?;

    match &inst {
        Some(inst) => store_vm_override(inst, provider, &cmd_token.to_string(), auth)?,
        None => upsert_proxy_entry(config_path, provider, &cmd_token.to_string(), auth)?,
    }

    report_setup(config_path, provider, vm, &cmd_token.to_string(), auth);
    Ok(())
}

/// The injection scheme for a provider. `OpenAI` keys are always sent as
/// `Authorization: Bearer`; Anthropic uses `x-api-key` for an API key and
/// `Bearer` for a `setup-token`.
fn auth_for(provider: Provider, api_key: bool) -> ProxyAuthScheme {
    match provider {
        Provider::Anthropic if api_key => ProxyAuthScheme::ApiKey,
        Provider::Anthropic | Provider::Openai => ProxyAuthScheme::Bearer,
    }
}

fn print_setup_guidance(provider: Provider, auth: ProxyAuthScheme) {
    match provider {
        Provider::Openai => eprintln!(
            "Paste your OpenAI API key (sk-...). It is stored in your chosen secret backend, \
             never as plaintext in the config, and injected as Authorization: Bearer upstream. \
             Codex subscription (auth.json) is not supported in proxy mode — use an API key."
        ),
        Provider::Anthropic if auth == ProxyAuthScheme::ApiKey => eprintln!(
            "Paste your Anthropic API key (sk-ant-...). It is stored in your chosen secret \
             backend, never as plaintext in the config."
        ),
        Provider::Anthropic => {
            eprintln!(
                "Run `claude setup-token` (needs a Claude subscription) to generate a long-lived, \
                 inference-scoped token (~1 year), then paste it below."
            );
            eprintln!(
                "It is stored in your chosen secret backend, never as plaintext in the config or \
                 in the guest."
            );
        }
    }
}

fn report_setup(
    config_path: &Path,
    provider: Provider,
    vm: Option<&str>,
    credential_cmd: &str,
    auth: ProxyAuthScheme,
) {
    match vm {
        Some(vm) => eprintln!(
            "\nStored a per-VM {} credential override for '{vm}' (auth = \"{}\").",
            provider.name(),
            auth_str(auth),
        ),
        None => eprintln!(
            "\nWrote {}:\n  [proxy.{}]\n  credential = \"{credential_cmd}\"\n  auth = \"{}\"",
            config_path.display(),
            provider.name(),
            auth_str(auth),
        ),
    }
    eprintln!(
        "\nProxy mode is now configured. It applies to remote-mode VMs on both backends; the raw \
         credential stays on the host and is injected upstream, never forwarded into the guest."
    );
}

/// Persist a per-VM credential override in the (already-resolved) instance's
/// `proxy.json`.
fn store_vm_override(
    inst: &crate::config::Instance,
    provider: Provider,
    credential_cmd: &str,
    auth: ProxyAuthScheme,
) -> Result<()> {
    let mut state = ProxyState::load_or_default(inst)?;
    *state.slot_mut(provider) = Some(ProxyUpstream {
        credential: Secret::new(credential_cmd.to_string()),
        auth,
    });
    state.save(inst)
}

/// The secret-store service name for a provider, optionally per-VM. A per-VM
/// override appends `-<vm>` so it never collides with the global default.
fn service_for(provider: Provider, vm: Option<&str>) -> String {
    let base = match provider {
        Provider::Anthropic => ANTHROPIC_SERVICE,
        Provider::Openai => OPENAI_SERVICE,
    };
    match vm {
        Some(vm) => format!("{base}-{vm}"),
        None => base.to_string(),
    }
}

/// The `auth` string written to config for a scheme. Must match
/// `ProxyAuthScheme`'s serde encoding (asserted in tests).
fn auth_str(auth: ProxyAuthScheme) -> &'static str {
    match auth {
        ProxyAuthScheme::ApiKey => "api_key",
        ProxyAuthScheme::Bearer => "bearer",
    }
}

/// Insert or replace `[proxy.<provider>]`'s `credential` + `auth` in the config
/// at `config_path`, preserving every other key. Round-trips through
/// `toml::Value` (comments are lost, all keys kept), under an flock so a
/// concurrent writer can't corrupt the file.
fn upsert_proxy_entry(
    config_path: &Path,
    provider: Provider,
    credential_cmd: &str,
    auth: ProxyAuthScheme,
) -> Result<()> {
    let _lock = crate::fs_util::lock_sibling(config_path)?;
    let mut doc = read_or_init_doc(config_path)?;

    let root = doc
        .as_table_mut()
        .context("config root is not a TOML table")?;
    let proxy = root
        .entry("proxy".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let proxy_tbl = proxy.as_table_mut().context("[proxy] is not a table")?;
    let entry = proxy_tbl
        .entry(provider.name().to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let entry_tbl = entry
        .as_table_mut()
        .with_context(|| format!("[proxy.{}] is not a table", provider.name()))?;
    entry_tbl.insert(
        "credential".to_string(),
        toml::Value::String(credential_cmd.to_string()),
    );
    entry_tbl.insert(
        "auth".to_string(),
        toml::Value::String(auth_str(auth).to_string()),
    );

    let serialized = toml::to_string_pretty(&doc).context("Failed to serialize config")?;
    // `coop proxy setup` always writes a `cmd:` indirection (not the secret),
    // so 0644 matches coop's convention; a literal (BYO) credential gets 0600.
    let mode = if credential_cmd.starts_with("cmd:") {
        0o644
    } else {
        0o600
    };
    crate::fs_util::atomic_write_with_mode(config_path, &serialized, mode)
        .with_context(|| format!("Failed to write {}", config_path.display()))
}

fn read_or_init_doc(path: &Path) -> Result<toml::Value> {
    if path.exists() {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        toml::from_str::<toml::Value>(&s)
            .with_context(|| format!("Failed to parse {}", path.display()))
    } else {
        Ok(toml::Value::Table(toml::Table::new()))
    }
}

// ── status ───────────────────────────────────────────────────

/// Where a VM's effective credential for a provider comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// A per-VM override in the instance's `proxy.json`.
    Override,
    /// The `[proxy.<provider>]` config default.
    Default,
    /// Neither configured — the proxy is off for this provider.
    Off,
}

/// The redacted resolution of one provider for one VM (or the defaults view).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolution {
    source: Source,
    auth: Option<ProxyAuthScheme>,
    credential: Option<String>,
}

fn run_status(cfg: &CoopConfig, vm: Option<&str>) -> Result<()> {
    let out = &mut std::io::stdout();
    if let Some(name) = vm {
        let name = InstanceName::new(name)
            .with_context(|| format!("'{name}' is not a valid instance name"))?;
        let inst = cfg.resolve_instance(Some(&name))?;
        let state = ProxyState::load_or_default(&inst)?;
        write_vm_status(out, inst.name.as_str(), &state, &cfg.proxy)?;
    } else {
        writeln!(out, "Credential proxy — defaults ([proxy] in config.toml):")?;
        for provider in Provider::ALL {
            let res = resolve(provider, None, &cfg.proxy);
            writeln!(out, "  {:<10} {}", provider.name(), describe(&res))?;
        }
        let mut overrides = Vec::new();
        for inst in cfg.list_instances()? {
            match ProxyState::load_or_default(&inst) {
                Ok(state) if state.anthropic.is_some() || state.openai.is_some() => {
                    overrides.push((inst, state));
                }
                Ok(_) => {}
                // Surface a corrupt override rather than silently reporting the
                // VM as "no override" (which would mislead the operator).
                Err(e) => tracing::warn!(
                    "Skipping '{}' in proxy status — unreadable proxy.json: {e:#}",
                    inst.name
                ),
            }
        }
        if overrides.is_empty() {
            writeln!(out, "\nNo per-VM overrides.")?;
        } else {
            writeln!(out, "\nPer-VM overrides:")?;
            for (inst, state) in overrides {
                write_vm_status(out, inst.name.as_str(), &state, &cfg.proxy)?;
            }
        }
    }
    writeln!(
        out,
        "\nValues are cmd: references, not secrets. Each running VM in remote model mode routes \
         its agents through the proxy; the raw credential stays on the host."
    )?;
    Ok(())
}

fn write_vm_status(
    out: &mut impl std::io::Write,
    vm: &str,
    state: &ProxyState,
    cfg: &ProxyConfig,
) -> Result<()> {
    writeln!(out, "  {vm}:")?;
    for provider in Provider::ALL {
        let res = resolve(provider, Some(state), cfg);
        writeln!(out, "    {:<10} {}", provider.name(), describe(&res))?;
    }
    Ok(())
}

/// Resolve a provider to its redacted [`Resolution`]: per-VM override →
/// config default → off. `state` is `None` for the defaults-only view.
fn resolve(provider: Provider, state: Option<&ProxyState>, cfg: &ProxyConfig) -> Resolution {
    let override_ = state.and_then(|s| match provider {
        Provider::Anthropic => s.anthropic.as_ref(),
        Provider::Openai => s.openai.as_ref(),
    });
    let default = match provider {
        Provider::Anthropic => cfg.anthropic.as_ref(),
        Provider::Openai => cfg.openai.as_ref(),
    };
    match override_.or(default) {
        Some(up) => Resolution {
            source: if override_.is_some() {
                Source::Override
            } else {
                Source::Default
            },
            auth: Some(up.auth),
            credential: Some(redact_credential(up.credential.expose())),
        },
        None => Resolution {
            source: Source::Off,
            auth: None,
            credential: None,
        },
    }
}

/// A one-line human description of a [`Resolution`].
fn describe(res: &Resolution) -> String {
    let src = match res.source {
        Source::Off => return "off (no default, no override)".to_string(),
        Source::Override => "override",
        Source::Default => "default",
    };
    let auth = res.auth.map_or("?", auth_str);
    let cred = res.credential.as_deref().unwrap_or("?");
    format!("{src} — {auth}, {cred}")
}

/// Show a `cmd:` reference verbatim (it is a command, not a secret) but never
/// a literal credential a user may have hand-written into config/state.
fn redact_credential(credential: &str) -> String {
    if credential.starts_with("cmd:") {
        credential.to_string()
    } else {
        "<literal credential, redacted>".to_string()
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn config_at(dir: &Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn upstream(cred: &str, auth: ProxyAuthScheme) -> ProxyUpstream {
        ProxyUpstream {
            credential: Secret::new(cred.to_string()),
            auth,
        }
    }

    #[test]
    fn auth_str_matches_serde_encoding() {
        // Guard against drift from ProxyAuthScheme's serde rename.
        for scheme in [ProxyAuthScheme::ApiKey, ProxyAuthScheme::Bearer] {
            let serde_str = toml::Value::try_from(scheme).unwrap();
            assert_eq!(serde_str.as_str(), Some(auth_str(scheme)));
        }
    }

    #[test]
    fn auth_for_openai_is_always_bearer() {
        assert_eq!(auth_for(Provider::Openai, false), ProxyAuthScheme::Bearer);
        assert_eq!(auth_for(Provider::Openai, true), ProxyAuthScheme::Bearer);
        assert_eq!(auth_for(Provider::Anthropic, true), ProxyAuthScheme::ApiKey);
        assert_eq!(
            auth_for(Provider::Anthropic, false),
            ProxyAuthScheme::Bearer
        );
    }

    #[test]
    fn service_for_namespaces_provider_and_vm() {
        assert_eq!(service_for(Provider::Anthropic, None), "coop-anthropic");
        assert_eq!(service_for(Provider::Openai, None), "coop-openai");
        assert_eq!(
            service_for(Provider::Openai, Some("dev")),
            "coop-openai-dev"
        );
        assert_eq!(
            service_for(Provider::Anthropic, Some("test")),
            "coop-anthropic-test"
        );
    }

    #[test]
    fn upsert_writes_provider_block_parseable_by_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = config_at(tmp.path(), "");
        upsert_proxy_entry(
            &path,
            Provider::Openai,
            "cmd:echo tok",
            ProxyAuthScheme::Bearer,
        )
        .unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        let up = cfg.proxy.openai.unwrap();
        assert_eq!(up.credential.expose(), "cmd:echo tok");
        assert_eq!(up.auth, ProxyAuthScheme::Bearer);
        // Anthropic default is untouched.
        assert!(cfg.proxy.anthropic.is_none());
    }

    #[test]
    fn upsert_preserves_other_provider_and_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = config_at(
            tmp.path(),
            "ssh_port = 2222\n\n[proxy.anthropic]\ncredential = \"cmd:a\"\nauth = \"bearer\"\n",
        );
        upsert_proxy_entry(&path, Provider::Openai, "cmd:o", ProxyAuthScheme::Bearer).unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(cfg.ssh_port.get(), 2222);
        assert_eq!(cfg.proxy.anthropic.unwrap().credential.expose(), "cmd:a");
        assert_eq!(cfg.proxy.openai.unwrap().credential.expose(), "cmd:o");
    }

    #[test]
    fn cmd_reference_config_is_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let path = config_at(tmp.path(), "");
        upsert_proxy_entry(
            &path,
            Provider::Anthropic,
            "cmd:echo tok",
            ProxyAuthScheme::Bearer,
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "a cmd: reference is not a secret");
    }

    #[test]
    fn literal_credential_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let path = config_at(tmp.path(), "");
        upsert_proxy_entry(
            &path,
            Provider::Anthropic,
            "sk-ant-literal",
            ProxyAuthScheme::ApiKey,
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a literal secret must be owner-only");
    }

    #[test]
    fn resolve_prefers_override_then_default_then_off() {
        let cfg = ProxyConfig {
            anthropic: Some(upstream("cmd:default", ProxyAuthScheme::Bearer)),
            ..Default::default()
        };
        let state = ProxyState {
            anthropic: Some(upstream("cmd:override", ProxyAuthScheme::ApiKey)),
            ..Default::default()
        };

        let over = resolve(Provider::Anthropic, Some(&state), &cfg);
        assert_eq!(over.source, Source::Override);
        assert_eq!(over.credential.as_deref(), Some("cmd:override"));

        let def = resolve(Provider::Anthropic, None, &cfg);
        assert_eq!(def.source, Source::Default);
        assert_eq!(def.credential.as_deref(), Some("cmd:default"));

        let off = resolve(Provider::Openai, Some(&state), &cfg);
        assert_eq!(off.source, Source::Off);
    }

    #[test]
    fn describe_redacts_literal_and_shows_cmd() {
        let mut cfg = ProxyConfig {
            openai: Some(upstream("cmd:op read x", ProxyAuthScheme::Bearer)),
            ..Default::default()
        };
        let shown = describe(&resolve(Provider::Openai, None, &cfg));
        assert!(
            shown.contains("cmd:op read x"),
            "cmd: ref should be shown: {shown}"
        );

        cfg.openai = Some(upstream("sk-secret-literal", ProxyAuthScheme::Bearer));
        let redacted = describe(&resolve(Provider::Openai, None, &cfg));
        assert!(
            !redacted.contains("sk-secret-literal"),
            "literal leaked: {redacted}"
        );
        assert!(redacted.contains("redacted"));
    }

    #[test]
    fn write_vm_status_lists_both_providers() {
        let cfg = ProxyConfig {
            anthropic: Some(upstream("cmd:a", ProxyAuthScheme::Bearer)),
            ..Default::default()
        };
        let state = ProxyState::default();
        let mut buf = Vec::new();
        write_vm_status(&mut buf, "dev", &state, &cfg).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("dev:"));
        assert!(text.contains("anthropic"));
        assert!(text.contains("openai"));
        assert!(
            text.contains("default"),
            "anthropic should resolve to default: {text}"
        );
        assert!(text.contains("off"), "openai should be off: {text}");
    }
}
