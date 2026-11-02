//! Startup configuration for `coop-proxy`, read once from stdin.
//!
//! `coop` resolves the real upstream credential on the host (via its `cmd:`
//! secret-resolution machinery) and hands the whole blob to the proxy over a
//! stdin pipe — never argv (world-readable through `/proc/<pid>/cmdline`) and
//! never a file on disk. The proxy deserializes it once at startup, closes
//! stdin, and holds the secret in process memory only.

use std::fmt;
use std::net::SocketAddr;

use serde::Deserialize;

/// A secret string that never appears in `Debug` output or logs.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Borrow the underlying value. Named to flag every read at review time.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// How the proxy injects the real upstream credential onto forwarded
/// requests. The guest's credential slot is always stripped first (see
/// [`crate::proxy`]); this decides the header that replaces it.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "scheme", rename_all = "snake_case")]
pub enum Injection {
    /// Anthropic API key → `x-api-key: <credential>`.
    XApiKey { credential: Secret },
    /// Bearer token (e.g. a Claude `setup-token`) →
    /// `authorization: Bearer <credential>`.
    Bearer { credential: Secret },
    /// Codex workspace PAT, with host-validated workspace routing metadata.
    CodexAccessToken {
        credential: Secret,
        account_id: String,
        is_fedramp: bool,
    },
}

/// The full startup blob. One `coop-proxy` process serves exactly one
/// upstream (per-integration proxy) — Codex spawns a second process with its
/// own upstream and token rather than a route table inside one process.
#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    /// Guest-visible listen address. Must be a concrete host interface bound
    /// to the private host-guest link, never an unspecified address
    /// (`0.0.0.0` / `[::]`) — enforced at bind time in [`crate::proxy::serve`].
    pub listen: SocketAddr,

    /// Per-instance capability token the guest must present on every request.
    /// Worthless off the host (it only authorizes the local proxy, which
    /// injects the real credential itself), so exfiltrating it gains a
    /// compromised guest nothing.
    pub capability_token: Secret,

    /// The fixed upstream host, e.g. `api.anthropic.com`. The guest controls
    /// only the request path — never this host or the `https` scheme — which
    /// is what closes SSRF: a rogue guest cannot retarget the injected key at
    /// an attacker-controlled host.
    pub upstream_host: String,

    /// The real credential and the header it is injected as.
    pub injection: Injection,
}

impl ProxyConfig {
    /// Parse the startup blob from a JSON string.
    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        let cfg: Self = serde_json::from_str(s)
            .map_err(|e| anyhow::anyhow!("Failed to parse proxy config: {e}"))?;
        // Defense in depth: an empty token would fail open, since
        // `constant_time_eq(b"", b"")` is true and a bare `Authorization:
        // Bearer ` presents an empty token. coop always mints a 64-hex token,
        // so this is unreachable in production, but reject it at parse anyway.
        if cfg.capability_token.expose().is_empty() {
            anyhow::bail!("capability_token must not be empty");
        }
        if let Injection::CodexAccessToken {
            credential,
            account_id,
            ..
        } = &cfg.injection
        {
            if cfg.upstream_host != "chatgpt.com"
                || !credential.expose().starts_with("at-")
                || credential.expose().len() <= 3
                || !credential
                    .expose()
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
                || account_id.is_empty()
                || account_id.len() > 256
                || !account_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                anyhow::bail!("invalid Codex access-token proxy configuration");
            }
        } else if cfg.upstream_host == "chatgpt.com" {
            anyhow::bail!("the Codex backend requires codex_access_token injection");
        }
        Ok(cfg)
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
#[expect(clippy::panic, reason = "tests use panic! for unreachable arms")]
mod tests {
    use super::*;

    fn sample_json() -> &'static str {
        r#"{
            "listen": "172.16.0.1:8788",
            "capability_token": "cap-token-abc",
            "upstream_host": "api.anthropic.com",
            "injection": { "scheme": "x_api_key", "credential": "sk-secret" }
        }"#
    }

    #[test]
    fn parses_api_key_injection() {
        let cfg = ProxyConfig::from_json(sample_json()).unwrap();
        assert_eq!(cfg.listen.port(), 8788);
        assert_eq!(cfg.upstream_host, "api.anthropic.com");
        assert_eq!(cfg.capability_token.expose(), "cap-token-abc");
        match &cfg.injection {
            Injection::XApiKey { credential } => assert_eq!(credential.expose(), "sk-secret"),
            _ => panic!("expected x_api_key"),
        }
    }

    #[test]
    fn parses_bearer_injection() {
        let json = r#"{
            "listen": "127.0.0.1:1",
            "capability_token": "t",
            "upstream_host": "api.anthropic.com",
            "injection": { "scheme": "bearer", "credential": "setup-tok" }
        }"#;
        let cfg = ProxyConfig::from_json(json).unwrap();
        match &cfg.injection {
            Injection::Bearer { credential } => assert_eq!(credential.expose(), "setup-tok"),
            _ => panic!("expected bearer"),
        }
    }

    fn codex_config() -> serde_json::Value {
        serde_json::json!({
            "listen": "127.0.0.1:9788",
            "capability_token": "cap-private",
            "upstream_host": "chatgpt.com",
            "injection": { "scheme": "codex_access_token", "credential": "at-private", "account_id": "workspace-123", "is_fedramp": false }
        })
    }

    #[test]
    fn codex_configuration_requires_its_fixed_backend_and_valid_headers() {
        let valid = codex_config();
        let cfg = ProxyConfig::from_json(&valid.to_string()).unwrap();
        assert!(!format!("{cfg:?}").contains("at-private"));
        for (len, accepted) in [(256, true), (257, false)] {
            let mut boundary = valid.clone();
            boundary["injection"]["account_id"] = "a".repeat(len).into();
            assert_eq!(
                ProxyConfig::from_json(&boundary.to_string()).is_ok(),
                accepted,
                "length {len}"
            );
        }
        for (key, value) in [
            ("account_id", ""),
            ("account_id", "workspace\r\nsecret"),
            ("credential", "at-private\nsecret"),
            ("credential", "sk-private"),
            ("credential", "at-"),
        ] {
            let mut invalid = valid.clone();
            invalid["injection"][key] = value.into();
            let err = ProxyConfig::from_json(&invalid.to_string())
                .unwrap_err()
                .to_string();
            assert!(!err.contains("secret"));
        }
        for host in ["api.openai.com", "attacker.invalid"] {
            let mut invalid = valid.clone();
            invalid["upstream_host"] = host.into();
            assert!(ProxyConfig::from_json(&invalid.to_string()).is_err());
        }
        let mut invalid = valid;
        invalid["injection"]["scheme"] = "bearer".into();
        assert!(ProxyConfig::from_json(&invalid.to_string()).is_err());
    }

    #[test]
    fn rejects_unknown_scheme() {
        let json = r#"{
            "listen": "127.0.0.1:1",
            "capability_token": "t",
            "upstream_host": "h",
            "injection": { "scheme": "basic", "credential": "x" }
        }"#;
        assert!(ProxyConfig::from_json(json).is_err());
    }

    #[test]
    fn rejects_empty_capability_token() {
        let json = r#"{
            "listen": "127.0.0.1:1",
            "capability_token": "",
            "upstream_host": "api.anthropic.com",
            "injection": { "scheme": "bearer", "credential": "x" }
        }"#;
        assert!(ProxyConfig::from_json(json).is_err());
    }

    #[test]
    fn secret_debug_is_redacted() {
        let cfg = ProxyConfig::from_json(sample_json()).unwrap();
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("sk-secret"),
            "credential leaked: {rendered}"
        );
        assert!(
            !rendered.contains("cap-token-abc"),
            "token leaked: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }
}
