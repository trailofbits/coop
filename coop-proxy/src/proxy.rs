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
//! - Only the provider-specific method/path operations required by the coding
//!   agents are forwarded; all other authenticated requests are refused
//!   locally before any upstream connection is made.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::{Body, Bytes, Frame, Incoming, SizeHint};
use hyper::header::{AUTHORIZATION, HOST, HeaderMap, HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
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

/// Bound guest sockets separately: idle connections never reach the request
/// semaphore, but still consume a host file descriptor and an HTTP task.
const MAX_CONCURRENT_CONNECTIONS: usize = 256;

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

// Only unit tests can replace the TLS transport after request preparation.
#[cfg(test)]
type TestBackend =
    fn(Request<Incoming>) -> Pin<Box<dyn std::future::Future<Output = Response<ProxyBody>> + Send>>;

/// Shared, cheaply-cloneable per-connection state.
#[derive(Clone)]
struct Ctx {
    cfg: Arc<ProxyConfig>,
    connector: TlsConnector,
    #[cfg(test)]
    test_backend: Option<TestBackend>,
    permits: Arc<Semaphore>,
    connections: Arc<Semaphore>,
}

impl Ctx {
    fn new(cfg: ProxyConfig) -> Result<Self> {
        let connector = TlsConnector::from(tls::client_config()?);
        Ok(Self {
            cfg: Arc::new(cfg),
            connector,
            #[cfg(test)]
            test_backend: None,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            connections: Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS)),
        })
    }
}

/// Bind the listener and serve until `shutdown` resolves.
///
/// Refuses to bind an unspecified address (`0.0.0.0` / `[::]`): the proxy
/// must be reachable only over the private host-guest link, never every host
/// interface.
///
/// `Ctx::new` runs before the bind so that a bound listener means the proxy can
/// serve — nothing fallible may sit between the bind and `accept_loop`.
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
                        // Resource exhaustion can make accept fail immediately;
                        // avoid spinning and filling the host log in that case.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let Ok(permit) = ctx.connections.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let _permit = permit;
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

    if !operation_allowed(req.method(), req.uri(), &ctx.cfg.upstream_host) {
        return Err(Refusal {
            status: StatusCode::FORBIDDEN,
            msg: "operation is not allowed by coop-proxy",
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
    parts.uri = upstream_uri(&parts.uri, &ctx.cfg.upstream_host).map_err(|_| Refusal {
        status: StatusCode::BAD_REQUEST,
        msg: "invalid request target",
    })?;
    let upstream_req = Request::from_parts(parts, body);

    forward(upstream_req, ctx, permit).await
}

/// Whether coop-proxy allows this method/path pair for the fixed upstream.
///
/// Provider operations are deliberately default-deny: the coding agents only
/// need response/message creation and Anthropic token counting, so
/// administrative APIs and stored-resource reads must never inherit the host
/// credential's broader authority. Agent upgrades that require another route
/// must fail with 403 until this policy, its tests, and the documentation are
/// deliberately updated. Codex currently identifies this custom provider as
/// `coop credential proxy`, not `OpenAI`, so its OpenAI-specific
/// `POST /v1/responses/compact` behavior does not apply.
fn operation_allowed(method: &Method, uri: &Uri, upstream_host: &str) -> bool {
    operation_allowed_bytes(
        method == Method::POST,
        upstream_host.as_bytes(),
        uri.path().as_bytes(),
    )
}

const MAX_ALLOWED_HOST_BYTES: usize = b"api.anthropic.com".len();
const MAX_ALLOWED_PATH_BYTES: usize = b"/v1/messages/count_tokens".len();

fn operation_allowed_bytes(is_post: bool, host: &[u8], path: &[u8]) -> bool {
    is_post
        && host.len() <= MAX_ALLOWED_HOST_BYTES
        && path.len() <= MAX_ALLOWED_PATH_BYTES
        && match host {
            b"api.openai.com" | b"chatgpt.com" => path == b"/v1/responses",
            b"api.anthropic.com" => path == b"/v1/messages" || path == b"/v1/messages/count_tokens",
            _ => false,
        }
}

async fn forward(
    req: Request<Incoming>,
    ctx: &Ctx,
    permit: OwnedSemaphorePermit,
) -> Result<Response<ProxyBody>, Refusal> {
    #[cfg(test)]
    if let Some(backend) = ctx.test_backend {
        return Ok(backend(req).await.map(|body| {
            GuardedBody {
                inner: body,
                _permit: permit,
            }
            .boxed()
        }));
    }

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

#[cfg(kani)]
mod proofs {
    use super::{
        MAX_ALLOWED_HOST_BYTES, MAX_ALLOWED_PATH_BYTES, constant_time_eq, operation_allowed_bytes,
    };

    // Arbitrary bytes include every valid HTTP host/path spelling and also
    // malformed spellings. Longer inputs cannot equal any allowed literal.
    #[kani::proof]
    fn operation_policy_is_exact_allowlist() {
        let host: [u8; MAX_ALLOWED_HOST_BYTES] = kani::any();
        let path: [u8; MAX_ALLOWED_PATH_BYTES] = kani::any();
        let host_len: usize = kani::any();
        let path_len: usize = kani::any();
        let is_post: bool = kani::any();
        kani::assume(host_len <= host.len() && path_len <= path.len());
        let host = &host[..host_len];
        let path = &path[..path_len];

        let allowed = operation_allowed_bytes(is_post, host, path);
        let openai_host = host == b"api.openai.com" || host == b"chatgpt.com";
        let anthropic_host = host == b"api.anthropic.com";
        let response = path == b"/v1/responses";
        let message = path == b"/v1/messages";
        let count = path == b"/v1/messages/count_tokens";
        assert_eq!(
            allowed,
            is_post && ((openai_host && response) || (anthropic_host && (message || count)))
        );
        assert!(operation_allowed_bytes(
            true,
            b"api.openai.com",
            b"/v1/responses"
        ));
        assert!(operation_allowed_bytes(
            true,
            b"chatgpt.com",
            b"/v1/responses"
        ));
        assert!(operation_allowed_bytes(
            true,
            b"api.anthropic.com",
            b"/v1/messages"
        ));
        assert!(operation_allowed_bytes(
            true,
            b"api.anthropic.com",
            b"/v1/messages/count_tokens"
        ));
        let overlong_host: [u8; MAX_ALLOWED_HOST_BYTES + 1] = kani::any();
        let overlong_path: [u8; MAX_ALLOWED_PATH_BYTES + 1] = kani::any();
        assert!(!operation_allowed_bytes(
            true,
            &overlong_host,
            b"/v1/responses"
        ));
        assert!(!operation_allowed_bytes(
            true,
            b"api.openai.com",
            &overlong_path
        ));
    }

    #[kani::proof]
    fn capability_token_comparison_matches_byte_equality() {
        let presented: [u8; 64] = kani::any();
        let expected: [u8; 64] = kani::any();
        let mut equal = true;
        for index in 0..64 {
            equal &= presented[index] == expected[index];
        }
        assert_eq!(constant_time_eq(&presented, &expected), equal);
        assert!(!constant_time_eq(&presented[..63], &expected));
        let longer: [u8; 65] = kani::any();
        assert!(!constant_time_eq(&longer, &expected));
    }
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
        if is_hop_by_hop(name)
            || name == HOST
            || is_credential_header(name)
            || (matches!(injection, Injection::CodexAccessToken { .. })
                && matches!(
                    name.as_str(),
                    "chatgpt-account-id" | "x-openai-fedramp" | "x-openai-actor-authorization"
                ))
        {
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
        Injection::Bearer { credential } | Injection::CodexAccessToken { credential, .. } => {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", credential.expose()))
                .context("credential is not a valid header value")?;
            value.set_sensitive(true);
            out.insert(AUTHORIZATION, value);
        }
    }

    if let Injection::CodexAccessToken {
        account_id,
        is_fedramp,
        ..
    } = injection
    {
        out.insert(
            "chatgpt-account-id",
            HeaderValue::from_str(account_id).context("invalid Codex workspace header")?,
        );
        if *is_fedramp {
            out.insert("x-openai-fedramp", HeaderValue::from_static("true"));
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

/// The guest uses the same Responses provider in both proxy modes. Only the
/// host-selected Codex profile translates its allowed inference operation;
/// no guest-supplied backend URL or workspace header controls the destination.
fn upstream_uri(uri: &Uri, upstream_host: &str) -> Result<Uri> {
    if upstream_host == "chatgpt.com" {
        anyhow::ensure!(uri.path() == "/v1/responses", "unsupported Codex operation");
        let mut path = "/backend-api/codex/responses".to_string();
        if let Some(query) = uri.query() {
            path.push('?');
            path.push_str(query);
        }
        return path.parse().context("invalid Codex request target");
    }
    origin_form(uri)
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

    #[tokio::test]
    #[expect(
        clippy::expect_used,
        reason = "test deadlines describe the missing behavior"
    )]
    async fn idle_connections_are_bounded_and_released_on_disconnect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let cfg = ProxyConfig::from_json(
            r#"{
                "listen": "127.0.0.1:0",
                "capability_token": "test-token",
                "upstream_host": "proxy-test.invalid",
                "injection": { "scheme": "x_api_key", "credential": "fake" }
            }"#,
        )
        .unwrap();
        let mut ctx = Ctx::new(cfg).unwrap();
        ctx.connections = Arc::new(Semaphore::new(2));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(accept_loop(ctx, listener, async {
            let _ = stopped.await;
        }));
        let budget = Duration::from_secs(5);
        let mut held = Vec::new();
        // A response proves each socket was accepted and entered HTTP serving.
        // Keep it open afterward: these idle sockets hold no request permits.
        for _ in 0..2 {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            timeout(budget, async {
                while !response.windows(2).any(|pair| pair == b"\r\n") {
                    assert_ne!(stream.read_buf(&mut response).await.unwrap(), 0);
                }
            })
            .await
            .unwrap();
            assert!(response.starts_with(b"HTTP/1.1 401 "));
            held.push(stream);
        }

        let mut excess = TcpStream::connect(addr).await.unwrap();
        let mut byte = [0];
        let n = timeout(budget, excess.read(&mut byte))
            .await
            .expect("excess idle connection must be closed without HTTP input")
            .unwrap();
        assert_eq!(n, 0);

        drop(held);
        // Disconnect releases capacity; wait for a real HTTP response, since
        // connect alone can succeed even while the accept loop refuses sockets.
        timeout(budget, async {
            loop {
                let mut stream = TcpStream::connect(addr).await.unwrap();
                if stream
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .await
                    .is_ok()
                {
                    let mut response = String::new();
                    if stream.read_to_string(&mut response).await.is_ok()
                        && response.contains("401")
                    {
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect must release connection capacity");
        let _ = stop.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn accepted_codex_request_is_forwarded_with_host_identity() {
        let cfg = ProxyConfig::from_json(
            r#"{
            "listen": "127.0.0.1:0",
            "capability_token": "guest-capability",
            "upstream_host": "chatgpt.com",
            "injection": { "scheme": "codex_access_token", "credential": "at-host-only",
                "account_id": "host-workspace", "is_fedramp": true }
        }"#,
        )
        .unwrap();
        let mut ctx = Ctx::new(cfg).unwrap();
        ctx.test_backend = Some(|req| {
            Box::pin(async move {
                assert_eq!(req.method(), Method::POST);
                assert_eq!(req.uri(), "/backend-api/codex/responses");
                let headers = req.headers();
                assert_eq!(headers[HOST], "chatgpt.com");
                assert_eq!(headers[AUTHORIZATION], "Bearer at-host-only");
                assert_eq!(headers["chatgpt-account-id"], "host-workspace");
                assert_eq!(headers["x-openai-fedramp"], "true");
                assert!(!headers.contains_key("x-openai-actor-authorization"));
                assert!(!headers.contains_key("x-api-key"));
                assert_eq!(
                    req.into_body().collect().await.unwrap().to_bytes(),
                    "{\"input\":\"hello\"}"
                );
                Response::new(
                    Full::new(Bytes::from_static(b"canned Codex response"))
                        .map_err(|never| match never {})
                        .boxed(),
                )
            })
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(accept_loop(ctx, listener, async {
            let _ = stopped.await;
        }));
        timeout(Duration::from_secs(5), async {
            let stream = TcpStream::connect(addr).await.unwrap();
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .unwrap();
            let connection = tokio::spawn(connection);
            let req = Request::post("/v1/responses")
                .header(HOST, "guest.invalid")
                .header(AUTHORIZATION, "Bearer guest-capability")
                .header("x-api-key", "guest-key")
                .header("chatgpt-account-id", "guest-workspace")
                .header("x-openai-fedramp", "false")
                .header("x-openai-actor-authorization", "guest-actor")
                .body(Full::new(Bytes::from_static(b"{\"input\":\"hello\"}")))
                .unwrap();
            let response = sender.send_request(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "canned Codex response"
            );
            drop(sender);
            connection.await.unwrap().unwrap();
        })
        .await
        .unwrap();
        stop.send(()).unwrap();
        server.await.unwrap();
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

    // ── Provider operation policy ──────────────────────────────

    #[test]
    fn openai_allows_only_response_creation() {
        let create: Uri = "/v1/responses".parse().unwrap();
        let create_with_query: Uri = "/v1/responses?foo=bar".parse().unwrap();

        assert!(operation_allowed(&Method::POST, &create, "api.openai.com"));
        assert!(operation_allowed(
            &Method::POST,
            &create_with_query,
            "api.openai.com"
        ));
    }

    #[test]
    fn codex_profile_allows_only_inference_and_pins_the_backend_path() {
        for path in [
            "/v1/responses",
            "/v1/responses?foo=bar",
            "https://attacker.invalid/v1/responses?foo=bar",
        ] {
            let uri: Uri = path.parse().unwrap();
            assert!(operation_allowed(&Method::POST, &uri, "chatgpt.com"));
            let expected = if uri.query().is_some() {
                "/backend-api/codex/responses?foo=bar"
            } else {
                "/backend-api/codex/responses"
            };
            assert_eq!(
                upstream_uri(&uri, "chatgpt.com").unwrap().to_string(),
                expected
            );
            assert_eq!(
                upstream_uri(&uri, "api.openai.com").unwrap(),
                origin_form(&uri).unwrap()
            );
            for method in [Method::GET, Method::DELETE, Method::HEAD, Method::CONNECT] {
                assert!(!operation_allowed(&method, &uri, "chatgpt.com"));
            }
        }
        for path in [
            "/backend-api/codex/responses",
            "/v1/responses/compact",
            "/v1/responses/resp_1",
            "/v1/models",
            "/backend-api/me",
            "/v1/messages",
            "/v1/responses/",
        ] {
            let uri: Uri = path.parse().unwrap();
            assert!(!operation_allowed(&Method::POST, &uri, "chatgpt.com"));
            assert!(upstream_uri(&uri, "chatgpt.com").is_err());
        }
    }

    #[test]
    fn codex_headers_replace_guest_identity_and_preserve_stream_metadata() {
        let mut incoming = HeaderMap::new();
        for (key, value) in [
            ("authorization", "Bearer cap-private"),
            ("x-api-key", "cap-private"),
            ("host", "attacker.invalid"),
            ("chatgpt-account-id", "attacker-workspace"),
            ("x-openai-fedramp", "true"),
            ("x-openai-actor-authorization", "attacker-identity"),
            ("content-type", "application/json"),
            ("originator", "codex_cli_rs"),
        ] {
            incoming.insert(HeaderName::from_bytes(key.as_bytes()).unwrap(), hv(value));
        }
        for is_fedramp in [false, true] {
            let injection: Injection = serde_json::from_value(serde_json::json!({ "scheme": "codex_access_token", "credential": "at-private", "account_id": "workspace-123", "is_fedramp": is_fedramp })).unwrap();
            let out = build_upstream_headers(&incoming, "chatgpt.com", &injection).unwrap();
            assert_eq!(out[AUTHORIZATION], "Bearer at-private");
            assert!(out[AUTHORIZATION].is_sensitive());
            assert_eq!(out[HOST], "chatgpt.com");
            assert_eq!(out["chatgpt-account-id"], "workspace-123");
            assert!(out.get("x-api-key").is_none());
            assert!(out.get("x-openai-actor-authorization").is_none());
            assert_eq!(out.get("x-openai-fedramp").is_some(), is_fedramp);
            if is_fedramp {
                assert_eq!(out["x-openai-fedramp"], "true");
            }
            assert_eq!(out["originator"], "codex_cli_rs");
            assert_eq!(out["content-type"], "application/json");
        }
    }

    #[test]
    fn openai_denies_admin_key_creation() {
        let uri: Uri = "/v1/organization/admin_api_keys".parse().unwrap();
        assert!(!operation_allowed(&Method::POST, &uri, "api.openai.com"));
    }

    #[test]
    fn openai_denies_stored_response_reads() {
        let uri: Uri = "/v1/responses/resp_123".parse().unwrap();
        assert!(!operation_allowed(&Method::GET, &uri, "api.openai.com"));
    }

    #[test]
    fn openai_policy_is_default_deny() {
        for (method, path) in [
            (Method::GET, "/v1/responses"),
            (Method::DELETE, "/v1/responses/resp_123"),
            (Method::POST, "/v1/files"),
            (Method::POST, "/v1/responses/"),
            (Method::TRACE, "/v1/responses"),
        ] {
            let uri: Uri = path.parse().unwrap();
            assert!(
                !operation_allowed(&method, &uri, "api.openai.com"),
                "unexpectedly allowed {method} {path}"
            );
        }
    }

    #[test]
    fn anthropic_allows_message_creation_and_token_counting() {
        for path in ["/v1/messages", "/v1/messages/count_tokens"] {
            let uri: Uri = path.parse().unwrap();
            assert!(operation_allowed(&Method::POST, &uri, "api.anthropic.com"));
        }
    }

    #[test]
    fn anthropic_policy_is_default_deny() {
        for (method, path) in [
            (Method::GET, "/v1/messages"),
            (Method::POST, "/v1/messages/batches"),
            (Method::GET, "/v1/messages/batches/msgbatch_123/results"),
            (Method::POST, "/v1/organizations/invites"),
            (Method::POST, "/v1/messages/"),
        ] {
            let uri: Uri = path.parse().unwrap();
            assert!(
                !operation_allowed(&method, &uri, "api.anthropic.com"),
                "unexpectedly allowed {method} {path}"
            );
        }
    }

    #[test]
    fn provider_operations_cannot_cross_provider_boundaries() {
        let openai_path: Uri = "/v1/responses".parse().unwrap();
        let anthropic_path: Uri = "/v1/messages".parse().unwrap();

        assert!(!operation_allowed(
            &Method::POST,
            &anthropic_path,
            "api.openai.com"
        ));
        assert!(!operation_allowed(
            &Method::POST,
            &openai_path,
            "api.anthropic.com"
        ));
    }

    #[test]
    fn unknown_upstream_has_no_allowed_operations() {
        let uri: Uri = "/v1/messages".parse().unwrap();
        assert!(!operation_allowed(
            &Method::POST,
            &uri,
            "proxy-test.invalid"
        ));
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
