//! End-to-end tests of the capability-token gate and the bind guard, driven
//! against the real `coop-proxy` binary — config fed over stdin, requests over
//! a loopback socket, exactly as `coop` runs it.
//!
//! These assert the security-critical refusal path without a live upstream: a
//! request that fails the capability check is rejected with 401 and the
//! upstream is never contacted; a request that passes fails closed at the
//! (unresolvable) upstream with 502, proving the gate opened without any real
//! credential reaching a real service. The authorized header-rewrite/injection
//! logic is covered by the unit tests in `src/proxy.rs`, and end-to-end against
//! a mock upstream by coop's VM integration suite.

#![expect(clippy::unwrap_used, reason = "tests")]
#![expect(clippy::expect_used, reason = "tests")]
#![expect(clippy::panic, reason = "tests")]

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_coop-proxy");

/// A config whose upstream can never resolve, so any request that passes the
/// gate fails closed rather than reaching a real service.
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

async fn free_loopback_addr() -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    addr
}

/// Spawn the proxy binary, feed it `listen` config on stdin, and wait until it
/// accepts connections on `addr`.
async fn spawn_serving(addr: SocketAddr) -> Child {
    let mut child = Command::new(BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(config_json(&addr.to_string()).as_bytes())
        .await
        .unwrap();
    drop(stdin);

    for _ in 0..200 {
        if TcpStream::connect(addr).await.is_ok() {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("proxy did not start listening on {addr}");
}

/// Send a minimal HTTP/1.1 request and return the status line.
async fn request_status(addr: SocketAddr, auth_header: Option<&str>) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut req = String::from("GET /v1/messages HTTP/1.1\r\nHost: proxy\r\n");
    if let Some(h) = auth_header {
        req.push_str(h);
        req.push_str("\r\n");
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf);
    text.lines().next().unwrap_or_default().to_string()
}

#[tokio::test]
async fn rejects_missing_token_with_401() {
    let addr = free_loopback_addr().await;
    let _child = spawn_serving(addr).await;
    let status = request_status(addr, None).await;
    assert!(status.contains("401"), "expected 401, got: {status:?}");
}

#[tokio::test]
async fn rejects_wrong_token_with_401() {
    let addr = free_loopback_addr().await;
    let _child = spawn_serving(addr).await;
    let status = request_status(addr, Some("Authorization: Bearer wrong-token")).await;
    assert!(status.contains("401"), "expected 401, got: {status:?}");
}

#[tokio::test]
async fn valid_token_passes_gate_and_fails_closed_at_upstream() {
    let addr = free_loopback_addr().await;
    let _child = spawn_serving(addr).await;
    let status = request_status(addr, Some("Authorization: Bearer the-right-token")).await;
    assert!(status.contains("502"), "expected 502, got: {status:?}");
}

#[tokio::test]
async fn refuses_to_bind_unspecified_address() {
    let mut child = Command::new(BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(config_json("0.0.0.0:0").as_bytes())
        .await
        .unwrap();
    drop(stdin);

    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("proxy should exit promptly on a bad bind address")
        .unwrap();
    assert!(!output.status.success(), "proxy should exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unspecified"),
        "expected unspecified-bind refusal, got: {stderr}"
    );
}
