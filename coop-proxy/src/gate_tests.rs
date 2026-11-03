//! HTTP gate tests run in-process so their ability to execute does not depend
//! on the host kernel's Landlock support. Production confinement is exercised
//! separately by the binary `--jail-selftest` integration gate.

#![expect(clippy::unwrap_used, reason = "tests")]
#![expect(clippy::panic, reason = "tests")]

use std::collections::BTreeSet;
use std::future;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::config::ProxyConfig;

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
const PORT_ATTEMPTS: usize = 10;

static HANDED_OUT: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());

struct Serving {
    addr: SocketAddr,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn config_json(listen: &str) -> String {
    format!(
        r#"{{
            "listen": "{listen}",
            "capability_token": "the-right-token",
            "upstream_host": "proxy-test.invalid",
            "injection": {{ "scheme": "x_api_key", "credential": "sk-should-never-leave" }}
        }}"#
    )
}

async fn bind_fresh() -> TcpListener {
    for _ in 0..PORT_ATTEMPTS {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        if HANDED_OUT.lock().unwrap().insert(port) {
            return listener;
        }
    }
    panic!("no unused loopback port after {PORT_ATTEMPTS} attempts");
}

async fn free_loopback_addr() -> SocketAddr {
    let listener = bind_fresh().await;
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

async fn spawn_serving() -> Serving {
    spawn_serving_with_config(config_json).await
}

async fn spawn_serving_with_config(config: fn(&str) -> String) -> Serving {
    for _ in 0..PORT_ATTEMPTS {
        let addr = free_loopback_addr().await;
        let cfg = ProxyConfig::from_json(&config(&addr.to_string())).unwrap();
        let task = tokio::spawn(crate::proxy::serve(cfg, future::pending()));
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if task.is_finished() {
                let error = task.await.unwrap().unwrap_err();
                let address_in_use = error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)
                });
                assert!(
                    address_in_use,
                    "proxy exited instead of serving on {addr}: {error:#}"
                );
                break;
            }
            if is_serving(addr).await {
                assert!(
                    !task.is_finished(),
                    "proxy exited during the readiness probe on {addr}"
                );
                return Serving { addr, task };
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "proxy did not begin serving on {addr} within {READY_TIMEOUT:?}"
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
    panic!("proxy lost the race for a free loopback port {PORT_ATTEMPTS} times");
}

async fn is_serving(addr: SocketAddr) -> bool {
    let Ok(stream) = TcpStream::connect(addr).await else {
        return false;
    };
    probe_reply(stream).await
}

// A connect-only probe can connect to itself on Linux if its ephemeral source
// port is the port being sampled. Require a distinct peer and a real HTTP
// status line so that this harness cannot report readiness before the proxy
// has bound its listener.
async fn probe_reply(stream: TcpStream) -> bool {
    let (Ok(local), Ok(peer)) = (stream.local_addr(), stream.peer_addr()) else {
        return false;
    };
    if local == peer {
        return false;
    }
    status_line(stream, PROBE_TIMEOUT)
        .await
        .is_some_and(|status| status.starts_with("HTTP/"))
}

async fn status_line(mut stream: TcpStream, read_budget: Duration) -> Option<String> {
    stream
        .write_all(b"GET /v1/messages HTTP/1.1\r\nHost: proxy\r\nConnection: close\r\n\r\n")
        .await
        .ok()?;
    let mut response = Vec::new();
    tokio::time::timeout(read_budget, stream.read_to_end(&mut response))
        .await
        .ok()?
        .ok()?;
    Some(
        String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
    )
}

async fn serve_one(reply: &'static [u8]) -> SocketAddr {
    let listener = bind_fresh().await;
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await;
            let _ = socket.write_all(reply).await;
        }
    });
    addr
}

#[tokio::test]
async fn readiness_probe_rejects_unbound_port() {
    assert!(!is_serving(free_loopback_addr().await).await);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn readiness_probe_rejects_self_connect_promptly() {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = socket.local_addr().unwrap();
    HANDED_OUT.lock().unwrap().insert(addr.port());
    let stream = socket.connect(addr).await.unwrap();
    assert_eq!(stream.local_addr().unwrap(), stream.peer_addr().unwrap());
    let Ok(verdict) = tokio::time::timeout(PROBE_TIMEOUT / 2, probe_reply(stream)).await else {
        panic!("self-connect should be rejected without waiting for a read timeout");
    };
    assert!(!verdict);
}

#[tokio::test]
async fn readiness_probe_rejects_echoed_request_line() {
    let addr = serve_one(b"GET /v1/messages HTTP/1.1\r\n\r\n").await;
    assert!(!is_serving(addr).await);
}

#[tokio::test]
async fn readiness_probe_accepts_http_status_line() {
    let addr = serve_one(b"HTTP/1.1 401 Unauthorized\r\n\r\n").await;
    assert!(is_serving(addr).await);
}

async fn request_status(addr: SocketAddr, auth_header: Option<&str>) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request =
        String::from("POST /v1/messages HTTP/1.1\r\nHost: proxy\r\nContent-Length: 0\r\n");
    if let Some(header) = auth_header {
        request.push_str(header);
        request.push_str("\r\n");
    }
    request.push_str("Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    tokio::time::timeout(RESPONSE_TIMEOUT, stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn rejects_missing_token_with_401() {
    let server = spawn_serving().await;
    let status = request_status(server.addr, None).await;
    assert!(status.contains("401"), "expected 401, got: {status:?}");
}

#[tokio::test]
async fn rejects_wrong_token_with_401() {
    let server = spawn_serving().await;
    let status = request_status(server.addr, Some("Authorization: Bearer wrong-token")).await;
    assert!(status.contains("401"), "expected 401, got: {status:?}");
}

#[tokio::test]
async fn valid_token_passes_auth_gate_and_reaches_operation_policy() {
    let server = spawn_serving().await;
    let status = request_status(server.addr, Some("Authorization: Bearer the-right-token")).await;
    assert!(status.contains("403"), "expected 403, got: {status:?}");
}

#[tokio::test]
async fn codex_profile_authenticates_capabilities_and_denies_other_operations() {
    let server = spawn_serving_with_config(|listen| {
        serde_json::json!({
            "listen": listen,
            "capability_token": "the-right-token",
            "upstream_host": "chatgpt.com",
            "injection": {
                "scheme": "codex_access_token",
                "credential": "at-host-only",
                "account_id": "workspace-123",
                "is_fedramp": false
            }
        })
        .to_string()
    })
    .await;

    for (method, path, token, expected) in [
        ("POST", "/v1/responses", "wrong-token", "401"),
        ("GET", "/v1/responses", "the-right-token", "403"),
        ("POST", "/v1/responses/compact", "the-right-token", "403"),
        (
            "POST",
            "/backend-api/codex/responses",
            "the-right-token",
            "403",
        ),
        ("GET", "/backend-api/me", "the-right-token", "403"),
        ("POST", "/v1/messages", "the-right-token", "403"),
    ] {
        let mut stream = TcpStream::connect(server.addr).await.unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: attacker.invalid\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(RESPONSE_TIMEOUT, stream.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected} ")),
            "unexpected response for {method} {path}: {response}"
        );
        assert!(!response.contains("at-host-only"));
    }
}

#[tokio::test]
async fn exits_when_its_port_is_taken() {
    let listener = bind_fresh().await;
    let addr = listener.local_addr().unwrap();
    let cfg = ProxyConfig::from_json(&config_json(&addr.to_string())).unwrap();
    let error = crate::proxy::serve(cfg, future::pending())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("failed to bind proxy listener"),
        "expected address-in-use error, got: {error:#}"
    );
}

#[tokio::test]
async fn refuses_to_bind_unspecified_address() {
    let cfg = ProxyConfig::from_json(&config_json("0.0.0.0:0")).unwrap();
    let error = crate::proxy::serve(cfg, future::pending())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("unspecified"),
        "expected unspecified-bind refusal, got: {error:#}"
    );
}
