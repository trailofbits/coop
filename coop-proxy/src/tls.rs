//! TLS for the proxy→upstream hop.
//!
//! The guest→proxy hop is plain HTTP on the private host-guest link (no TLS,
//! no CA — the guest is explicitly pointed at the proxy). Only this hop is
//! TLS: a normal outbound HTTPS connection to the pinned upstream, its
//! certificate verified against the compiled-in Mozilla root set
//! ([`webpki_roots`]). A bug that disabled this verification would MITM the
//! real credential, so the configuration is deliberately minimal and has no
//! path that skips verification.

use std::sync::Arc;

use anyhow::Result;
use rustls::{ClientConfig, RootCertStore};

/// Build the shared client TLS configuration: standard certificate
/// verification against the pinned `webpki-roots`, no client auth.
pub fn client_config() -> Result<Arc<ClientConfig>> {
    // Install the aws-lc-rs provider as the process default so the builder
    // below (and any downstream construction) resolves it unambiguously even
    // though it is the only provider compiled in.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if roots.is_empty() {
        anyhow::bail!("no trust anchors compiled in — refusing to run without a root store");
    }

    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    Ok(Arc::new(config))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn client_config_builds_with_pinned_roots() {
        let cfg = client_config().unwrap();
        // Sanity: the shared config resolves and carries the ALPN/versions
        // defaults. The presence of a non-empty root store is asserted at
        // build time (bail above); this proves construction succeeds.
        assert!(Arc::strong_count(&cfg) >= 1);
    }
}
