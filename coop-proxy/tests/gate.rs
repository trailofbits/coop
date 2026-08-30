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

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
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

/// How many ports to try before giving up (see [`spawn_serving`]).
const PORT_ATTEMPTS: usize = 10;

/// Ports already handed out in this process. `bind(0)` can return a port a
/// sibling test just released, and these tests run concurrently in one binary,
/// so without this two of them would race to give the same port to two proxies.
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

/// A loopback port that is free *right now* and not yet used by this process.
/// Nothing holds it, so it can still be taken before the child binds — see
/// [`spawn_serving`].
async fn free_loopback_addr() -> SocketAddr {
    loop {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let fresh = HANDED_OUT.lock().unwrap().insert(addr.port());
        if fresh {
            return addr;
        }
    }
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
                // Kill first: `drain_stderr` reads to EOF, which only arrives
                // once the child closes the pipe.
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
        // Pin both so the child's stderr is what the harness expects whatever
        // the developer's environment: `LC_ALL` keeps the `EADDRINUSE` text
        // untranslated for the retry check below, and `RUST_LOG` keeps the
        // volume to the couple of lines that fit the undrained pipe buffer.
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
                // A probe only proves *something* answers on `addr`. If our
                // child lost the port it is already gone, and the answer came
                // from whoever won it.
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
///
/// A bare `connect()` is not a readiness signal: the kernel can give this
/// probe's own outgoing socket the very port it is probing, and
/// `127.0.0.1:X → 127.0.0.1:X` is a TCP simultaneous open that succeeds with
/// nothing listening, then echoes back whatever the probe writes. Such a socket
/// has `peer_addr() == local_addr()`; requiring a reply that *starts with*
/// `HTTP/` rejects it a second time, since the echo of the request line merely
/// contains that text.
///
/// Every failure is a `false`, never a panic — this runs during the startup
/// window, where a peer may reset the connection at any point, and the retry
/// loop is what should absorb that.
async fn is_serving(addr: SocketAddr) -> bool {
    let Ok(stream) = TcpStream::connect(addr).await else {
        return false;
    };
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
        .unwrap_or_default()
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let _ = sock.write_all(reply).await;
        }
    });
    addr
}

#[tokio::test]
async fn readiness_probe_rejects_a_dead_port() {
    let addr = free_loopback_addr().await;
    assert!(!is_serving(addr).await);
}

#[tokio::test]
async fn readiness_probe_rejects_an_echoed_request_line() {
    // What a self-connected socket returns. It contains "HTTP/" but does not
    // start with it, which is why `is_serving` matches on the prefix.
    let addr = serve_one(b"GET /v1/messages HTTP/1.1\r\n\r\n").await;
    assert!(!is_serving(addr).await);
}

#[tokio::test]
async fn readiness_probe_accepts_an_http_status_line() {
    let addr = serve_one(b"HTTP/1.1 401 Unauthorized\r\n\r\n").await;
    assert!(is_serving(addr).await);
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

    let output = tokio::time::timeout(RESPONSE_TIMEOUT, child.wait_with_output())
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
