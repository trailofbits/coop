#![cfg(unix)]
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test fixtures and bounded-time failure reporting"
)]

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn hidden_proxy_command_handshakes_relays_both_directions_and_exits() {
    const PAYLOAD_LEN: usize = 256 * 1024;

    let directory = tempfile::tempdir().expect("tempdir");
    let socket = directory.path().join("vsock.sock");
    let listener = UnixListener::bind(&socket).expect("bind fixture socket");
    let (request_sender, request_receiver) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().expect("accept proxy");
        let mut request = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            connection.read_exact(&mut byte).expect("read handshake");
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        assert_eq!(request, b"CONNECT 22\n");
        connection.write_all(b"OK 1073741824\n").expect("ack");
        // Fill the guest-to-host direction while independently reading the
        // host request. A blocking stdout write deadlocks here because the test
        // does not drain stdout until the complete request reaches the server.
        let mut response_connection = connection.try_clone().expect("clone fixture socket");
        let response_writer = std::thread::spawn(move || {
            response_connection
                .write_all(&vec![0x5a; PAYLOAD_LEN])
                .expect("write response");
        });
        let mut request = vec![0_u8; PAYLOAD_LEN];
        connection.read_exact(&mut request).expect("read request");
        assert!(request.iter().all(|byte| *byte == 0xa5));
        request_sender.send(()).expect("report complete request");
        response_writer.join().expect("response writer");
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_coop"))
        .args([
            "__vsock-proxy",
            "--socket",
            socket.to_str().expect("UTF-8 fixture path"),
            "--port",
            "22",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start proxy");
    let mut child_stdin = child.stdin.take().expect("proxy stdin");
    let writer = std::thread::spawn(move || {
        child_stdin
            .write_all(&vec![0xa5; PAYLOAD_LEN])
            .expect("write proxy stdin");
    });
    if let Err(error) = request_receiver.recv_timeout(Duration::from_secs(5)) {
        child.kill().expect("kill stalled proxy");
        child.wait().expect("reap stalled proxy");
        let _ = writer.join();
        let _ = server.join();
        panic!("proxy did not relay closed stdin under stdout backpressure: {error}");
    }
    writer.join().expect("writer thread");

    // Hold stdin at EOF and stdout backpressured long enough to exercise the
    // state where inactive descriptors must not keep poll immediately ready.
    std::thread::sleep(Duration::from_millis(200));
    let mut child_stdout = child.stdout.take().expect("proxy stdout");
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut response = vec![0_u8; PAYLOAD_LEN];
        sender
            .send(child_stdout.read_exact(&mut response).map(|()| response))
            .expect("send proxy response");
    });
    let response = match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(response) => response.expect("read response"),
        Err(error) => {
            child.kill().expect("kill stalled proxy");
            child.wait().expect("reap stalled proxy");
            reader.join().expect("reader thread");
            let _ = server.join();
            panic!("proxy did not flush interactive response: {error}");
        }
    };
    reader.join().expect("reader thread");
    assert!(response.iter().all(|byte| *byte == 0x5a));
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll proxy") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill proxy that did not exit");
            child.wait().expect("reap proxy that did not exit");
            panic!("proxy did not exit after guest EOF");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .expect("proxy stderr")
        .read_to_end(&mut stderr)
        .expect("read proxy stderr");
    server.join().expect("server thread");

    assert!(status.success(), "{}", String::from_utf8_lossy(&stderr));
}
