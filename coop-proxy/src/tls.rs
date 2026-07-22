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

    // A self-signed `CN=localhost` cert + its PKCS#8 key, minted once with
    // openssl (valid to 2126) so the test needs no cert-generation crate. It
    // chains to no pinned root, so a correct verifier must reject it.
    const UNTRUSTED_CERT_DER: &[u8] = include_bytes!("testdata/untrusted_upstream_cert.der");
    const UNTRUSTED_KEY_DER: &[u8] = include_bytes!("testdata/untrusted_upstream_key.pkcs8.der");

    // The Tier-1 property: the proxy→upstream hop must reject a certificate
    // that does not chain to the pinned roots. Stand up a TLS server with the
    // untrusted self-signed cert and assert the config built by
    // `client_config()` refuses the handshake. A regression that disabled
    // verification (e.g. a `dangerous()` verifier) would let the bad cert
    // through and fail this test.
    #[tokio::test]
    async fn rejects_untrusted_upstream_cert() {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
        use tokio::net::{TcpListener, TcpStream};
        use tokio_rustls::{TlsAcceptor, TlsConnector};

        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let server_cfg = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(UNTRUSTED_CERT_DER)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(UNTRUSTED_KEY_DER)),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_cfg));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = acceptor.accept(stream).await;
            }
        });

        let connector = TlsConnector::from(client_config().unwrap());
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = ServerName::try_from("localhost").unwrap();
        assert!(
            connector.connect(name, tcp).await.is_err(),
            "self-signed upstream cert must be rejected by the pinned-root verifier"
        );
    }
}
