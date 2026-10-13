#![cfg(target_os = "linux")]
#![expect(clippy::unwrap_used, reason = "test assertions")]

use std::ffi::OsStr;
use std::fs;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;

#[test]
fn stop_retains_proxy_for_full_socket_queue_and_cleans_after_close() {
    let root = tempfile::tempdir().unwrap();
    // The runner's umask may let tempfile create a group-writable directory.
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
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
    let sudo_log = root.path().join("sudo-probe.log");
    let docker_fixture =
        std::env::var_os("COOP_LINUX_UNIT_TEST_CONTAINER").as_deref() == Some(OsStr::new("1"));
    let sudo_override = if docker_fixture {
        let tools = root.path().join("bin");
        fs::create_dir(&tools).unwrap();
        let sudo = tools.join("sudo");
        // Docker cannot sudo-exec another user's /proc/<pid>/exe with its
        // default capabilities. The socket belongs to this test user.
        fs::write(
            &sudo,
            "#!/bin/sh\n\
             if [ \"$#\" -ge 2 ] && [ \"$2\" = '__probe-firecracker-socket' ]; then\n\
               case \"$1\" in\n\
                 /proc/*/exe) printf '%s\\n' \"$1\" >> \"$COOP_TEST_SUDO_LOG\"; exec \"$@\" ;;\n\
                 *) exit 97 ;;\n\
               esac\n\
             fi\n\
             exec \"$COOP_TEST_REAL_SUDO\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&sudo, fs::Permissions::from_mode(0o700)).unwrap();
        let original_path = std::env::var_os("PATH").unwrap();
        let real_sudo = std::env::split_paths(&original_path)
            .map(|dir| dir.join("sudo"))
            .find(|path| {
                fs::metadata(path)
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
            .unwrap();
        let mut search_path = vec![tools];
        search_path.extend(std::env::split_paths(&original_path));
        Some((std::env::join_paths(search_path).unwrap(), real_sudo))
    } else {
        None
    };
    let socket = instance.join("firecracker.socket");
    let listener = UnixListener::bind(&socket).unwrap();
    // Linux permits backlog + 1 pending connections. With backlog zero,
    // one connected client fills the queue without relying on system defaults.
    // SAFETY: listener owns a valid listening socket descriptor.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let client = UnixStream::connect(&socket).unwrap();
    let stop = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_coop"));
        command
            .args(["--config"])
            .arg(&config)
            .args(["stop", "test"])
            .env("HOME", root.path());
        if let Some((search_path, real_sudo)) = &sudo_override {
            command
                .env("PATH", search_path)
                .env("COOP_TEST_SUDO_LOG", &sudo_log)
                .env("COOP_TEST_REAL_SUDO", real_sudo);
        }
        command.output().unwrap()
    };
    let probe_count = || fs::read_to_string(&sudo_log).map_or(0, |probes| probes.lines().count());

    // Both a missing PID and a PID belonging to an exited process must
    // leave the live listener's resources alone.
    for stale_pid in [false, true] {
        if stale_pid {
            let mut child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            assert!(child.wait().unwrap().success());
            fs::write(instance.join("firecracker.pid"), pid.to_string()).unwrap();
        }
        let before = probe_count();
        let output = stop();
        if docker_fixture {
            assert!(probe_count() > before);
        }
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
    let before = probe_count();
    let output = stop();
    if docker_fixture {
        assert!(probe_count() > before);
    }
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!token.exists());
    if docker_fixture {
        let probes = fs::read_to_string(&sudo_log).unwrap();
        assert!(
            probes
                .lines()
                .all(|path| path.starts_with("/proc/") && path.ends_with("/exe"))
        );
    }
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
