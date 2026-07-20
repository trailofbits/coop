//! `coop proxy` subcommands (issue #411).
//!
//! `coop proxy setup` mirrors the GitHub PAT wizard: take an Anthropic
//! credential (a Claude `setup-token` by default, or an API key with
//! `--api-key`) pasted at the prompt, store it in a secret backend of the
//! user's choice, and write the resulting `cmd:` reference into
//! `[proxy.anthropic]` — so the credential is never a plaintext value in the
//! config, and (in proxy mode) never enters the guest.

#![expect(
    clippy::print_stderr,
    reason = "setup wizard is interactive CLI — stderr is user communication"
)]

use std::path::Path;

use anyhow::{Context, Result};

use crate::ProxyAction;
use crate::config::{CoopConfig, ProxyAuthScheme};
use crate::github_pat::{pick_backend, read_token_no_echo};
use crate::secret_store::{self, ANTHROPIC_SERVICE, AccountName};

/// Account name for the single Anthropic proxy credential. The file backend
/// stores it at `<state>/anthropic/anthropic.txt` (service `coop-anthropic`).
const ANTHROPIC_ACCOUNT: &str = "anthropic";

pub(crate) fn cmd_proxy(cfg: &CoopConfig, config_path: &Path, action: &ProxyAction) -> Result<()> {
    match action {
        ProxyAction::Setup { api_key } => run_setup(cfg, config_path, *api_key),
    }
}

fn run_setup(cfg: &CoopConfig, config_path: &Path, api_key: bool) -> Result<()> {
    let auth = if api_key {
        ProxyAuthScheme::ApiKey
    } else {
        ProxyAuthScheme::Bearer
    };
    print_setup_guidance(api_key);

    let token = read_token_no_echo()?;
    let backend = pick_backend()?;

    let account = AccountName::new(ANTHROPIC_ACCOUNT)
        .context("internal error: invalid proxy account name")?;
    let state_dir = cfg.data_dir.join("state");
    let cmd_token =
        secret_store::store_secret(backend, ANTHROPIC_SERVICE, &account, &token, &state_dir)
            .context("Failed to store the Anthropic credential in the chosen backend")?;

    upsert_proxy_entry(config_path, &cmd_token.to_string(), auth)?;

    eprintln!(
        "\nWrote {}:\n  [proxy.anthropic]\n  credential = \"{cmd_token}\"\n  auth = \"{}\"",
        config_path.display(),
        auth_str(auth),
    );
    eprintln!(
        "\nProxy mode is now configured. It applies to remote-mode VMs on the \
         Firecracker backend; the raw credential stays on the host and is \
         injected upstream, never forwarded into the guest."
    );
    Ok(())
}

fn print_setup_guidance(api_key: bool) {
    if api_key {
        eprintln!(
            "Paste your Anthropic API key (sk-ant-...). It is stored in your chosen secret \
             backend, never as plaintext in the config."
        );
    } else {
        eprintln!(
            "Run `claude setup-token` (needs a Claude subscription) to generate a long-lived, \
             inference-scoped token (~1 year), then paste it below."
        );
        eprintln!(
            "It is stored in your chosen secret backend, never as plaintext in the config or in \
             the guest."
        );
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

/// Insert or replace `[proxy.anthropic]`'s `credential` + `auth` in the config
/// at `config_path`, preserving every other key. Round-trips through
/// `toml::Value` (comments are lost, all keys kept), under an flock so a
/// concurrent writer can't corrupt the file.
fn upsert_proxy_entry(
    config_path: &Path,
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
    let anthropic = proxy_tbl
        .entry("anthropic".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let anthropic_tbl = anthropic
        .as_table_mut()
        .context("[proxy.anthropic] is not a table")?;
    anthropic_tbl.insert(
        "credential".to_string(),
        toml::Value::String(credential_cmd.to_string()),
    );
    anthropic_tbl.insert(
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

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn config_at(dir: &Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        path
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
    fn upsert_writes_proxy_block_parseable_by_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = config_at(tmp.path(), "");
        upsert_proxy_entry(&path, "cmd:echo tok", ProxyAuthScheme::Bearer).unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        let up = cfg.proxy.anthropic.unwrap();
        assert_eq!(up.credential.expose(), "cmd:echo tok");
        assert_eq!(up.auth, ProxyAuthScheme::Bearer);
    }

    #[test]
    fn upsert_creates_config_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        assert!(!path.exists());
        upsert_proxy_entry(&path, "cmd:x", ProxyAuthScheme::Bearer).unwrap();
        assert!(CoopConfig::load(&path).unwrap().proxy.is_enabled());
    }

    #[test]
    fn upsert_preserves_existing_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = config_at(tmp.path(), "ssh_port = 2222\n\n[claude]\n");
        upsert_proxy_entry(&path, "cmd:sec get", ProxyAuthScheme::ApiKey).unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        assert_eq!(cfg.ssh_port.get(), 2222);
        assert_eq!(cfg.proxy.anthropic.unwrap().auth, ProxyAuthScheme::ApiKey);
    }

    #[test]
    fn upsert_replaces_prior_proxy_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = config_at(
            tmp.path(),
            "[proxy.anthropic]\ncredential = \"cmd:old\"\nauth = \"bearer\"\n",
        );
        upsert_proxy_entry(&path, "cmd:new", ProxyAuthScheme::ApiKey).unwrap();

        let cfg = CoopConfig::load(&path).unwrap();
        let up = cfg.proxy.anthropic.unwrap();
        assert_eq!(up.credential.expose(), "cmd:new");
        assert_eq!(up.auth, ProxyAuthScheme::ApiKey);
    }

    #[test]
    fn cmd_reference_config_is_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let path = config_at(tmp.path(), "");
        upsert_proxy_entry(&path, "cmd:echo tok", ProxyAuthScheme::Bearer).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "a cmd: reference is not a secret");
    }

    #[test]
    fn literal_credential_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let path = config_at(tmp.path(), "");
        upsert_proxy_entry(&path, "sk-ant-literal", ProxyAuthScheme::ApiKey).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a literal secret must be owner-only");
    }
}
