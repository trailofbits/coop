#![cfg(target_os = "linux")]
#![expect(clippy::unwrap_used, reason = "test assertions")]

use std::fs;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;

#[test]
fn stop_retains_proxy_for_full_socket_queue_and_cleans_after_close() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let instance = data.join("instances/test");
    fs::create_dir_all(&instance).unwrap();
    fs::write(
        instance.join("instance.json"),
        r#"{"name":"test","index":0}"#,
    )
    .unwrap();
    let token = instance.join("proxy-openai.token");
    fs::write(&token, "proxy sentinel").unwrap();
    let config = root.path().join("config.toml");
    // This test exercises socket-state and proxy cleanup, not TAP mutation.
    // Use a root-owned exact probe that always reports network objects absent
    // instead of relying on PATH interception, which production forbids.
    fs::write(
        &config,
        format!("data_dir = {data:?}\n[network.host_tools]\nip = '/bin/false'\n"),
    )
    .unwrap();
    let socket = instance.join("firecracker.socket");
    let listener = UnixListener::bind(&socket).unwrap();
    // Linux permits backlog + 1 pending connections. With backlog zero,
    // one connected client fills the queue without relying on system defaults.
    // SAFETY: listener owns a valid listening socket descriptor.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let client = UnixStream::connect(&socket).unwrap();
    let stop = || {
        Command::new(env!("CARGO_BIN_EXE_coop"))
            .args(["--config"])
            .arg(&config)
            .args(["stop", "test"])
            .env("HOME", root.path())
            .output()
            .unwrap()
    };

    // Both a missing PID and a PID belonging to an exited process must
    // leave the live listener's resources alone.
    for stale_pid in [false, true] {
        if stale_pid {
            let mut child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            assert!(child.wait().unwrap().success());
            fs::write(instance.join("firecracker.pid"), pid.to_string()).unwrap();
        }
        let output = stop();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{error}");
        assert!(error.contains("os error 11"), "{error}");
        assert_eq!(fs::read_to_string(&token).unwrap(), "proxy sentinel");
    }

    // A closed listener leaves the socket path behind. The same command must
    // now clean up the proxy resource. Network teardown has dedicated tests
    // because production network-tool lookup intentionally ignores PATH.
    drop(client);
    drop(listener);
    let output = stop();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!token.exists());
}

#[test]
fn socket_helper_distinguishes_live_abandoned_and_missing_sockets() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("api.socket");
    let probe = || {
        Command::new(env!("CARGO_BIN_EXE_coop"))
            .arg("__probe-firecracker-socket")
            .arg(&socket)
            .output()
            .unwrap()
    };
    assert!(probe().status.success());
    let listener = UnixListener::bind(&socket).unwrap();
    let live = probe();
    assert!(!live.status.success());
    assert!(
        String::from_utf8_lossy(&live.stderr).contains("accepting connections"),
        "{}",
        String::from_utf8_lossy(&live.stderr)
    );
    drop(listener);
    assert!(socket.exists());
    assert!(probe().status.success());
}
