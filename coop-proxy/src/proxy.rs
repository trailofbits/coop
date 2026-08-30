//! The reverse-proxy request path: verify the guest's capability token,
//! rewrite headers (strip the guest credential, inject the real one, pin the
//! upstream `Host`), and stream the request/response bodies to the fixed
//! upstream over TLS.
//!
//! Everything here is deliberately small and explicit — it is the one
//! component that terminates a connection originated by the untrusted guest
//! and attaches the real credential. The security-critical invariants:
//!
//! - The guest is never authorized unless it presents the exact per-instance
//!   capability token (constant-time compared).
//! - The upstream host and scheme are fixed by the host-side config; only the
//!   request path is taken from the guest (closes SSRF).
//! - The guest's own `authorization` / `x-api-key` (the capability token) is
//!   stripped and replaced with the real credential — it never reaches the
//!   upstream, and the real credential never reaches the guest.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::{Body, Bytes, Frame, Incoming, SizeHint};
use hyper::header::{AUTHORIZATION, HOST, HeaderMap, HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use rustls::pki_types::ServerName;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Duration, timeout};
use tokio_rustls::TlsConnector;

use crate::config::{Injection, ProxyConfig};
use crate::tls;

/// Fixed upstream port. The guest cannot influence host or port — only the
/// request path is forwarded.
const UPSTREAM_PORT: u16 = 443;

/// How long to wait for the upstream TCP connect + TLS handshake before
/// giving up, so a rogue guest cannot pin host resources on a stuck dial.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on concurrently in-flight proxied requests, so a rogue guest cannot
/// exhaust host CPU/memory by opening unbounded upstream connections.
const MAX_CONCURRENT_REQUESTS: usize = 256;

/// The unified response body: either an upstream stream or a small local
/// error page, both boxed to one type.
type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// A response body that holds its concurrency permit until the body is fully
/// streamed. Without this the permit would drop when the response *headers*
/// arrive, leaving the (possibly minutes-long, streaming) body uncounted — so
/// `MAX_CONCURRENT_REQUESTS` would not actually bound concurrent upstream
/// connections. Attaching the permit here makes the cap effective for a
/// request's whole lifetime.
struct GuardedBody {
    inner: ProxyBody,
    _permit: OwnedSemaphorePermit,
}

impl Body for GuardedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The `x-api-key` header, constructed once.
fn x_api_key() -> HeaderName {
    HeaderName::from_static("x-api-key")
}

/// Shared, cheaply-cloneable per-connection state.
#[derive(Clone)]
struct Ctx {
    cfg: Arc<ProxyConfig>,
    connector: TlsConnector,
    permits: Arc<Semaphore>,
}

impl Ctx {
    fn new(cfg: ProxyConfig) -> Result<Self> {
        let connector = TlsConnector::from(tls::client_config()?);
        Ok(Self {
            cfg: Arc::new(cfg),
            connector,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        })
    }
}

/// Bind the listener and serve until `shutdown` resolves.
///
/// Refuses to bind an unspecified address (`0.0.0.0` / `[::]`): the proxy
/// must be reachable only over the private host-guest link, never every host
/// interface.
///
/// The `Ctx` is built before the bind, so a bound listener means the proxy can
/// serve: nothing fallible remains between the bind and `accept_loop`.
pub async fn serve(
    cfg: ProxyConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let listen = cfg.listen;
    if listen.ip().is_unspecified() {
        anyhow::bail!(
            "refusing to bind unspecified address {listen} — the proxy must bind only the \
             private host-guest interface"
        );
    }
    let ctx = Ctx::new(cfg)?;
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind proxy listener on {listen}"))?;
    tracing::info!(
        "coop-proxy listening on {listen} → https://{}",
        ctx.cfg.upstream_host
    );
    accept_loop(ctx, listener, shutdown).await;
    Ok(())
}

async fn accept_loop(
    ctx: Ctx,
    listener: TcpListener,
    shutdown: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => {
                tracing::info!("shutdown requested — no longer accepting connections");
                break;
            }
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _peer)) => stream,
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        continue;
                    }
                };
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let ctx = ctx.clone();
                        async move { handle(req, ctx).await }
                    });
                    if let Err(e) = ServerBuilder::new(TokioExecutor::new())
                        .serve_connection(io, service)
                        .await
                    {
                        tracing::debug!("guest connection error: {e}");
                    }
                });
            }
        }
    }
}

/// A refusal carrying only a fixed, secret-free message.
struct Refusal {
    status: StatusCode,
    msg: &'static str,
}

async fn handle(req: Request<Incoming>, ctx: Ctx) -> Result<Response<ProxyBody>, Infallible> {
    match proxy(req, &ctx).await {
        Ok(resp) => Ok(resp),
        Err(Refusal { status, msg }) => Ok(error_response(status, msg)),
    }
}

async fn proxy(req: Request<Incoming>, ctx: &Ctx) -> Result<Response<ProxyBody>, Refusal> {
    if !authorized(req.headers(), ctx.cfg.capability_token.expose()) {
        return Err(Refusal {
            status: StatusCode::UNAUTHORIZED,
            msg: "missing or invalid coop-proxy capability token",
        });
    }

    // Held until the response body finishes streaming (see `GuardedBody`), so
    // the concurrency cap bounds a request for its whole lifetime.
    let permit = ctx
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| Refusal {
            status: StatusCode::SERVICE_UNAVAILABLE,
            msg: "proxy at capacity",
        })?;

    let (mut parts, body) = req.into_parts();
    parts.headers =
        build_upstream_headers(&parts.headers, &ctx.cfg.upstream_host, &ctx.cfg.injection)
            .map_err(|_| Refusal {
                status: StatusCode::BAD_REQUEST,
                msg: "request headers could not be rewritten",
            })?;
    parts.uri = origin_form(&parts.uri).map_err(|_| Refusal {
        status: StatusCode::BAD_REQUEST,
        msg: "invalid request target",
    })?;
    let upstream_req = Request::from_parts(parts, body);

    forward(upstream_req, ctx, permit).await
}

async fn forward(
    req: Request<Incoming>,
    ctx: &Ctx,
    permit: OwnedSemaphorePermit,
) -> Result<Response<ProxyBody>, Refusal> {
    let host = ctx.cfg.upstream_host.as_str();
    let server_name = ServerName::try_from(host.to_owned()).map_err(|_| Refusal {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        msg: "configured upstream host is not a valid TLS server name",
    })?;

    let connect = async {
        let tcp = TcpStream::connect((host, UPSTREAM_PORT)).await?;
        let _ = tcp.set_nodelay(true);
        ctx.connector.connect(server_name, tcp).await
    };
    let tls_stream = match timeout(UPSTREAM_TIMEOUT, connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!("upstream connect/TLS to {host} failed: {e}");
            return Err(bad_gateway());
        }
        Err(_) => {
            tracing::warn!("upstream connect/TLS to {host} timed out");
            return Err(bad_gateway());
        }
    };

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls_stream))
        .await
        .map_err(|e| {
            tracing::warn!("upstream handshake failed: {e}");
            bad_gateway()
        })?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("upstream connection closed: {e}");
        }
    });

    let resp = sender.send_request(req).await.map_err(|e| {
        tracing::warn!("upstream request failed: {e}");
        bad_gateway()
    })?;
    // Wrap the streaming body so the permit is released only when the body is
    // fully drained, not when these headers arrive.
    Ok(resp.map(|body| {
        GuardedBody {
            inner: body.boxed(),
            _permit: permit,
        }
        .boxed()
    }))
}

fn bad_gateway() -> Refusal {
    Refusal {
        status: StatusCode::BAD_GATEWAY,
        msg: "upstream request failed",
    }
}

/// Whether the request presents the exact capability token. Constant-time on
/// the token bytes so a timing side-channel cannot recover it.
fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    match presented_token(headers) {
        Some(token) => constant_time_eq(&token, expected.as_bytes()),
        None => false,
    }
}

/// Extract the capability token the guest presented, from either
/// `Authorization: Bearer <t>` (Claude Code with `ANTHROPIC_AUTH_TOKEN`) or
/// `x-api-key: <t>`. Both slots are stripped before forwarding.
fn presented_token(headers: &HeaderMap) -> Option<Vec<u8>> {
    if let Some(value) = headers.get(AUTHORIZATION)
        && let Ok(s) = value.to_str()
        && let Some(rest) = s.strip_prefix("Bearer ")
    {
        return Some(rest.as_bytes().to_vec());
    }
    headers.get(x_api_key()).map(|v| v.as_bytes().to_vec())
}

/// Constant-time byte equality. The early length check leaks only length,
/// which for a fixed-width random token reveals nothing useful.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Build the header set sent upstream: drop hop-by-hop headers, drop the
/// guest's `Host` and credential slots, pin the upstream `Host`, and inject
/// the real credential (marked sensitive so hyper never logs it).
fn build_upstream_headers(
    incoming: &HeaderMap,
    upstream_host: &str,
    injection: &Injection,
) -> Result<HeaderMap> {
    let mut out = HeaderMap::with_capacity(incoming.len() + 1);
    for (name, value) in incoming {
        if is_hop_by_hop(name) || name == HOST || is_credential_header(name) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }

    out.insert(
        HOST,
        HeaderValue::from_str(upstream_host)
            .context("upstream host is not a valid header value")?,
    );

    match injection {
        Injection::XApiKey { credential } => {
            let mut value = HeaderValue::from_str(credential.expose())
                .context("credential is not a valid header value")?;
            value.set_sensitive(true);
            out.insert(x_api_key(), value);
        }
        Injection::Bearer { credential } => {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", credential.expose()))
                .context("credential is not a valid header value")?;
            value.set_sensitive(true);
            out.insert(AUTHORIZATION, value);
        }
    }

    Ok(out)
}

/// Whether a header carries a client credential we must strip before
/// forwarding (the guest's capability token arrives in one of these).
fn is_credential_header(name: &HeaderName) -> bool {
    name == AUTHORIZATION || name.as_str() == "x-api-key"
}

/// Connection-scoped headers that must not be forwarded across the proxy hop.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    const HOP_BY_HOP: [&str; 9] = [
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "proxy-authenticate",
        "proxy-authorization",
    ];
    HOP_BY_HOP.contains(&name.as_str())
}

/// Reduce a request URI to origin form (path + query only) for the upstream
/// HTTP/1.1 request line; the upstream host is carried by the `Host` header.
fn origin_form(uri: &Uri) -> Result<Uri> {
    let pq = uri.path_and_query().map_or("/", |p| p.as_str());
    pq.parse::<Uri>().context("invalid path-and-query")
}

fn error_response(status: StatusCode, msg: &'static str) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from_static(msg.as_bytes()))
        .map_err(|never: Infallible| match never {})
        .boxed();
    #[expect(
        clippy::expect_used,
        reason = "status/body are constants; builder cannot fail"
    )]
    Response::builder()
        .status(status)
        .body(body)
        .expect("static error response is always valid")
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    fn api_key_injection(secret: &str) -> Injection {
        let json = format!(r#"{{ "scheme": "x_api_key", "credential": "{secret}" }}"#);
        serde_json::from_str(&json).unwrap()
    }

    fn bearer_injection(secret: &str) -> Injection {
        let json = format!(r#"{{ "scheme": "bearer", "credential": "{secret}" }}"#);
        serde_json::from_str(&json).unwrap()
    }

    // ── capability token ─────────────────────────────────────

    #[test]
    fn constant_time_eq_matches_and_differs() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn presented_token_reads_bearer() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, hv("Bearer tok-123"));
        assert_eq!(presented_token(&h), Some(b"tok-123".to_vec()));
    }

    #[test]
    fn presented_token_reads_x_api_key() {
        let mut h = HeaderMap::new();
        h.insert(x_api_key(), hv("tok-456"));
        assert_eq!(presented_token(&h), Some(b"tok-456".to_vec()));
    }

    #[test]
    fn presented_token_bearer_wins_over_x_api_key() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, hv("Bearer from-auth"));
        h.insert(x_api_key(), hv("from-xapi"));
        assert_eq!(presented_token(&h), Some(b"from-auth".to_vec()));
    }

    #[test]
    fn presented_token_none_when_absent_or_wrong_scheme() {
        let mut h = HeaderMap::new();
        assert_eq!(presented_token(&h), None);
        h.insert(AUTHORIZATION, hv("Basic abc"));
        assert_eq!(presented_token(&h), None);
    }

    #[test]
    fn authorized_gate() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, hv("Bearer right"));
        assert!(authorized(&h, "right"));
        assert!(!authorized(&h, "wrong"));
        assert!(!authorized(&HeaderMap::new(), "right"));
    }

    // ── header rewrite ───────────────────────────────────────

    #[test]
    fn rewrite_strips_guest_credentials_and_injects_api_key() {
        let mut incoming = HeaderMap::new();
        incoming.insert(AUTHORIZATION, hv("Bearer capability-token"));
        incoming.insert("x-api-key", hv("capability-token"));
        incoming.insert("anthropic-version", hv("2023-06-01"));
        incoming.insert("content-type", hv("application/json"));
        incoming.insert(HOST, hv("172.16.0.1:8788"));

        let out = build_upstream_headers(
            &incoming,
            "api.anthropic.com",
            &api_key_injection("sk-real"),
        )
        .unwrap();

        assert_eq!(out.get("x-api-key").unwrap(), "sk-real");
        assert!(
            out.get(AUTHORIZATION).is_none(),
            "guest bearer must be stripped"
        );
        assert_eq!(out.get(HOST).unwrap(), "api.anthropic.com");
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(out.get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn rewrite_injects_bearer_and_replaces_guest_authorization() {
        let mut incoming = HeaderMap::new();
        incoming.insert(AUTHORIZATION, hv("Bearer capability-token"));
        let out = build_upstream_headers(
            &incoming,
            "api.anthropic.com",
            &bearer_injection("setup-tok"),
        )
        .unwrap();
        assert_eq!(out.get(AUTHORIZATION).unwrap(), "Bearer setup-tok");
        assert!(out.get("x-api-key").is_none());
    }

    #[test]
    fn rewrite_marks_injected_credential_sensitive() {
        let mut incoming = HeaderMap::new();
        incoming.insert(AUTHORIZATION, hv("Bearer t"));
        let out = build_upstream_headers(
            &incoming,
            "api.anthropic.com",
            &api_key_injection("sk-real"),
        )
        .unwrap();
        assert!(out.get("x-api-key").unwrap().is_sensitive());
    }

    #[test]
    fn rewrite_drops_hop_by_hop_headers() {
        let mut incoming = HeaderMap::new();
        incoming.insert(AUTHORIZATION, hv("Bearer t"));
        incoming.insert("connection", hv("keep-alive"));
        incoming.insert("keep-alive", hv("timeout=5"));
        incoming.insert("proxy-authorization", hv("Basic xyz"));
        let out = build_upstream_headers(&incoming, "api.anthropic.com", &api_key_injection("k"))
            .unwrap();
        assert!(out.get("connection").is_none());
        assert!(out.get("keep-alive").is_none());
        assert!(out.get("proxy-authorization").is_none());
    }

    #[test]
    fn rewrite_overrides_guest_host_even_without_incoming_host() {
        let incoming = HeaderMap::new();
        let out = build_upstream_headers(&incoming, "api.anthropic.com", &api_key_injection("k"))
            .unwrap();
        assert_eq!(out.get(HOST).unwrap(), "api.anthropic.com");
    }

    // ── uri origin form ──────────────────────────────────────

    #[test]
    fn origin_form_keeps_path_and_query() {
        let uri: Uri = "http://172.16.0.1:8788/v1/messages?beta=true"
            .parse()
            .unwrap();
        assert_eq!(
            origin_form(&uri).unwrap().to_string(),
            "/v1/messages?beta=true"
        );
    }

    #[test]
    fn origin_form_defaults_empty_path_to_root() {
        let uri: Uri = "http://172.16.0.1:8788".parse().unwrap();
        assert_eq!(origin_form(&uri).unwrap().to_string(), "/");
    }
}
