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
        serde_json::from_str(s).map_err(|e| anyhow::anyhow!("Failed to parse proxy config: {e}"))
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
            Injection::Bearer { .. } => panic!("expected x_api_key"),
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
            Injection::XApiKey { .. } => panic!("expected bearer"),
        }
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
