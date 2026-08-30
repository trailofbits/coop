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
//!
//! The `readiness_probe_*` tests are the exception: they drive no binary and no
//! gate, but pin the harness's own readiness probe, whose failure modes are
//! what made this file flaky (issue 435).

#![expect(clippy::unwrap_used, reason = "tests")]
#![expect(clippy::expect_used, reason = "tests")]
#![expect(clippy::panic, reason = "tests")]

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_coop-proxy");

/// How long a freshly spawned proxy gets to answer its first request.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How often to probe while waiting for that first answer.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Read budget for one readiness probe. Well under [`READY_TIMEOUT`] so that a
/// peer which accepts but never replies costs one poll, not the whole budget —
/// the loop keeps checking whether the child died.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Read budget for a test's own request, which waits on the real upstream path.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

/// How many ports to try before giving up (see [`spawn_serving`], [`bind_fresh`]).
const PORT_ATTEMPTS: usize = 10;

/// Every loopback port this process has taken, so no two tests in this binary
/// are handed the same one — `bind(0)` will otherwise return a port a sibling
/// just released, and these tests run concurrently. Only [`bind_fresh`] adds to
/// it, so every port in the binary comes from there.
static HANDED_OUT: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());

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

/// How a spawned proxy left the readiness wait: it answered, it exited first,
/// or it outlived [`READY_TIMEOUT`] without answering.
enum Startup {
    Serving,
    Exited,
    Unresponsive,
}

/// A listener on a loopback port no other test in this process has been given.
async fn bind_fresh() -> TcpListener {
    for _ in 0..PORT_ATTEMPTS {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let fresh = HANDED_OUT.lock().unwrap().insert(port);
        if fresh {
            return listener;
        }
    }
    panic!("no unused loopback port after {PORT_ATTEMPTS} attempts");
}

/// A loopback port that is free *right now*. Nothing holds it once this
/// returns, so it can still be taken before the child binds — see
/// [`spawn_serving`].
async fn free_loopback_addr() -> SocketAddr {
    let listener = bind_fresh().await;
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

/// Spawn the proxy binary on a free loopback port and wait until it answers
/// requests, returning the address it is serving on.
///
/// The port cannot be reserved for the child: between `free_loopback_addr`
/// sampling it and the child binding it, the kernel can hand the same port to
/// another socket — including this process's own outgoing probe connections —
/// and the child then exits with `EADDRINUSE`. Retrying on a fresh port is what
/// keeps that from flaking.
async fn spawn_serving() -> (SocketAddr, Child) {
    let mut last_stderr = String::new();
    for _ in 0..PORT_ATTEMPTS {
        let addr = free_loopback_addr().await;
        let mut child = spawn_proxy(addr).await;
        match wait_until_serving(&mut child, addr).await {
            Startup::Serving => return (addr, child),
            Startup::Unresponsive => {
                let _ = child.kill().await;
                let stderr = drain_stderr(&mut child).await;
                panic!("proxy did not answer on {addr} within {READY_TIMEOUT:?}: {stderr}");
            }
            Startup::Exited => {
                last_stderr = drain_stderr(&mut child).await;
                assert!(
                    last_stderr.contains("Address already in use"),
                    "proxy exited instead of serving on {addr}: {last_stderr}"
                );
            }
        }
    }
    panic!("proxy lost the race for a free loopback port {PORT_ATTEMPTS} times: {last_stderr}");
}

async fn spawn_proxy(addr: SocketAddr) -> Child {
    // `--no-jail`: this test drives the gate logic, not the jail, and runs on
    // CI hosts that may lack Landlock (where the fail-closed jail would abort
    // startup). coop never passes this flag; the jail is asserted separately by
    // `--jail-selftest` in the VM integration suite.
    let mut child = Command::new(BIN)
        .arg("--no-jail")
        // LC_ALL keeps the EADDRINUSE text untranslated for `spawn_serving`'s
        // retry check; RUST_LOG bounds the volume on this never-drained pipe.
        .env("LC_ALL", "C")
        .env("RUST_LOG", "info")
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
                // A probe only proves *something* answers on `addr`. This
                // establishes that our child had not already lost the port as
                // of this poll — not that it is the one answering.
                if child.try_wait().unwrap().is_some() {
                    return Startup::Exited;
                }
                return Startup::Serving;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    };
    tokio::time::timeout(READY_TIMEOUT, poll)
        .await
        .unwrap_or(Startup::Unresponsive)
}

/// Call only once the child has exited or been killed — this reads to EOF, and
/// a live child holds the pipe open.
async fn drain_stderr(child: &mut Child) -> String {
    let mut buf = Vec::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_end(&mut buf).await;
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Whether a proxy is answering requests on `addr`.
async fn is_serving(addr: SocketAddr) -> bool {
    let Ok(stream) = TcpStream::connect(addr).await else {
        return false;
    };
    probe_reply(stream).await
}

/// Whether `stream` reached a serving proxy.
///
/// A successful `connect()` is not enough: the kernel can give this probe's own
/// outgoing socket the very port it is probing, and `127.0.0.1:X →
/// 127.0.0.1:X` is a TCP simultaneous open that succeeds with nothing
/// listening, then echoes back whatever the probe writes.
///
/// Two checks reject that. `peer_addr() == local_addr()` identifies it up
/// front, which is what keeps it cheap: a self-connect never sends EOF, so
/// without that check every probe of a stolen port would wait out
/// [`PROBE_TIMEOUT`]. Requiring a reply that *starts with* `HTTP/` is the
/// backstop, since the echoed request line merely contains that text.
///
/// Every failure is a `false`, never a panic — this runs during the startup
/// window, where a peer may reset the connection at any point, and the retry
/// loop is what should absorb that.
async fn probe_reply(stream: TcpStream) -> bool {
    let (Ok(local), Ok(peer)) = (stream.local_addr(), stream.peer_addr()) else {
        return false;
    };
    if local == peer {
        return false;
    }
    status_line(stream, None, PROBE_TIMEOUT)
        .await
        .is_some_and(|status| status.starts_with("HTTP/"))
}

async fn request_status(addr: SocketAddr, auth_header: Option<&str>) -> String {
    let stream = TcpStream::connect(addr).await.unwrap();
    status_line(stream, auth_header, RESPONSE_TIMEOUT)
        .await
        .expect("request to the proxy failed at the socket level")
}

/// Send a minimal HTTP/1.1 request over `stream` and return the status line, or
/// `None` if the exchange failed at the socket level.
async fn status_line(
    mut stream: TcpStream,
    auth_header: Option<&str>,
    read_budget: Duration,
) -> Option<String> {
    let mut req = String::from("GET /v1/messages HTTP/1.1\r\nHost: proxy\r\n");
    if let Some(h) = auth_header {
        req.push_str(h);
        req.push_str("\r\n");
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.ok()?;

    let mut buf = Vec::new();
    let _ = tokio::time::timeout(read_budget, stream.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf);
    Some(text.lines().next().unwrap_or_default().to_string())
}

/// Accept once on a fresh loopback port, reply with `reply`, and close.
async fn serve_one(reply: &'static [u8]) -> SocketAddr {
    let listener = bind_fresh().await;
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            // Read the request first: closing with data still queued makes the
            // kernel send RST, which on BSD-derived stacks discards the reply.
            let mut discard = [0u8; 1024];
            let _ = sock.read(&mut discard).await;
            let _ = sock.write_all(reply).await;
        }
    });
    addr
}

#[tokio::test]
async fn readiness_probe_rejects_unbound_port() {
    let addr = free_loopback_addr().await;
    assert!(!is_serving(addr).await);
}

#[tokio::test]
async fn readiness_probe_rejects_self_connect() {
    // Force the simultaneous open that issue 435 hit by accident: a socket
    // connecting to its own bound address completes with nothing listening.
    // Binding `:0` and reading the port back keeps the socket in possession of
    // it throughout — sampling a free port first would reintroduce the very
    // steal this file exists to eliminate.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = socket.local_addr().unwrap();
    let stream = socket.connect(addr).await.unwrap();
    assert_eq!(
        stream.local_addr().unwrap(),
        stream.peer_addr().unwrap(),
        "expected a self-connected socket"
    );

    // Rejected on the peer check rather than by waiting out the read budget.
    // The `starts_with("HTTP/")` backstop would also reject the echo, so only
    // the promptness distinguishes the two — and promptness is the point: a
    // self-connect never sends EOF.
    let verdict = tokio::time::timeout(PROBE_TIMEOUT / 2, probe_reply(stream))
        .await
        .expect("a self-connect should be rejected without waiting out the read budget");
    assert!(!verdict);
}

#[tokio::test]
async fn readiness_probe_rejects_echoed_request_line() {
    // The self-connect echo: contains "HTTP/" but does not start with it.
    let addr = serve_one(b"GET /v1/messages HTTP/1.1\r\n\r\n").await;
    assert!(!is_serving(addr).await);
}

#[tokio::test]
async fn readiness_probe_accepts_http_status_line() {
    let addr = serve_one(b"HTTP/1.1 401 Unauthorized\r\n\r\n").await;
    assert!(is_serving(addr).await);
}

#[tokio::test]
async fn spawned_proxy_exits_when_its_port_is_taken() {
    // Hold the port for the child's whole life, so its bind cannot succeed.
    let listener = bind_fresh().await;
    let addr = listener.local_addr().unwrap();
    let mut child = spawn_proxy(addr).await;
    match wait_until_serving(&mut child, addr).await {
        Startup::Exited => {}
        Startup::Serving => panic!("proxy reported serving on an already-bound port"),
        Startup::Unresponsive => panic!("proxy neither served nor exited on a taken port"),
    }
    let stderr = drain_stderr(&mut child).await;
    assert!(
        stderr.contains("Address already in use"),
        "expected the EADDRINUSE text `spawn_serving` retries on, got: {stderr}"
    );
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
    // support (see spawn_proxy).
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
