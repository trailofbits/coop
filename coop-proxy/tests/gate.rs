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

/// How long a freshly spawned proxy gets to answer its first request.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How often to probe while waiting for that first answer.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How many ports to try before giving up (see [`spawn_serving`]).
const PORT_ATTEMPTS: usize = 10;

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

/// How a spawned proxy left the readiness wait.
enum Startup {
    /// It answered a request on its address.
    Serving,
    /// It exited before answering.
    Exited,
    /// It never answered within [`READY_TIMEOUT`].
    Unresponsive,
}

/// A loopback port that is free *right now*.
///
/// Nothing holds it: binding and dropping only samples the kernel's free list,
/// so the port can be taken again before the caller uses it. [`spawn_serving`]
/// handles losing that race.
async fn free_loopback_addr() -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    addr
}

/// Spawn the proxy binary on a free loopback port and wait until it answers
/// requests, returning the address it is serving on.
///
/// The port cannot be reserved for the child. `free_loopback_addr` yields a
/// port from the ephemeral range, and until the child binds it the kernel may
/// hand that same port to any other socket — including this process's own
/// outgoing probe connections, which do not set `SO_REUSEADDR` and so block the
/// child's listening bind. The child then exits with `EADDRINUSE`; retrying on
/// a fresh port is what makes this deterministic. A child that fails for any
/// other reason fails the test immediately, with its stderr.
async fn spawn_serving() -> (SocketAddr, Child) {
    for _ in 0..PORT_ATTEMPTS {
        let addr = free_loopback_addr().await;
        let mut child = spawn_proxy(addr).await;
        match wait_until_serving(&mut child, addr).await {
            Startup::Serving => return (addr, child),
            Startup::Unresponsive => {
                panic!("proxy did not answer on {addr} within {READY_TIMEOUT:?}")
            }
            Startup::Exited => {
                let stderr = drain_stderr(&mut child).await;
                assert!(
                    stderr.contains("Address already in use"),
                    "proxy exited instead of serving on {addr}: {stderr}"
                );
            }
        }
    }
    panic!("proxy lost the race for a free loopback port {PORT_ATTEMPTS} times");
}

/// Spawn the proxy binary and feed it the config for `addr` on stdin.
async fn spawn_proxy(addr: SocketAddr) -> Child {
    // `--no-jail`: this test drives the gate logic, not the jail, and runs on
    // CI hosts that may lack Landlock (where the fail-closed jail would abort
    // startup). coop never passes this flag; the jail is asserted separately by
    // `--jail-selftest` in the VM integration suite.
    let mut child = Command::new(BIN)
        .arg("--no-jail")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(config_json(&addr.to_string()).as_bytes())
        .await
        .unwrap();
    drop(stdin);
    child
}

/// Poll `addr` until the proxy answers, the child exits, or time runs out.
async fn wait_until_serving(child: &mut Child, addr: SocketAddr) -> Startup {
    let poll = async {
        loop {
            if child.try_wait().unwrap().is_some() {
                return Startup::Exited;
            }
            if is_serving(addr).await {
                return Startup::Serving;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    };
    tokio::time::timeout(READY_TIMEOUT, poll)
        .await
        .unwrap_or(Startup::Unresponsive)
}

/// Read the exited child's stderr to EOF.
async fn drain_stderr(child: &mut Child) -> String {
    let mut buf = Vec::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_end(&mut buf).await.unwrap();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Whether a proxy is answering requests on `addr`.
///
/// A bare `connect()` is not a readiness signal. `addr` is an ephemeral-range
/// port, so before the child binds the kernel can hand that same port to this
/// probe's own outgoing socket; `127.0.0.1:X → 127.0.0.1:X` is a TCP
/// simultaneous open that connects with nothing listening, and echoes back
/// whatever the probe writes. Rejecting a peer that is our own local address,
/// and requiring an HTTP status line in reply, separates a serving proxy from
/// that self-connect.
async fn is_serving(addr: SocketAddr) -> bool {
    let Ok(stream) = TcpStream::connect(addr).await else {
        return false;
    };
    if stream.local_addr().unwrap() == stream.peer_addr().unwrap() {
        return false;
    }
    status_line(stream, None).await.starts_with("HTTP/")
}

/// Send a minimal HTTP/1.1 request and return the status line.
async fn request_status(addr: SocketAddr, auth_header: Option<&str>) -> String {
    status_line(TcpStream::connect(addr).await.unwrap(), auth_header).await
}

/// Send a minimal HTTP/1.1 request over `stream` and return the status line.
async fn status_line(mut stream: TcpStream, auth_header: Option<&str>) -> String {
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
    let (addr, _child) = spawn_serving().await;
    let status = request_status(addr, None).await;
    assert!(status.contains("401"), "expected 401, got: {status:?}");
}

#[tokio::test]
async fn rejects_wrong_token_with_401() {
    let (addr, _child) = spawn_serving().await;
    let status = request_status(addr, Some("Authorization: Bearer wrong-token")).await;
    assert!(status.contains("401"), "expected 401, got: {status:?}");
}

#[tokio::test]
async fn valid_token_passes_gate_and_fails_closed_at_upstream() {
    let (addr, _child) = spawn_serving().await;
    let status = request_status(addr, Some("Authorization: Bearer the-right-token")).await;
    assert!(status.contains("502"), "expected 502, got: {status:?}");
}

#[tokio::test]
async fn refuses_to_bind_unspecified_address() {
    // `--no-jail` so the bind guard is asserted regardless of host Landlock
    // support (see spawn_serving).
    let mut child = Command::new(BIN)
        .arg("--no-jail")
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
