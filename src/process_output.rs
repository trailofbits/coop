//! Bounded output capture for host subprocesses.

use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CapturePolicy {
    pub(crate) stdout_max: usize,
    pub(crate) stderr_max: usize,
    pub(crate) completion: CompletionPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompletionPolicy {
    Deadline(Duration),
    UntilExit,
}

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

#[derive(Debug, Error)]
pub(crate) enum CaptureError {
    #[error("process interrupted by signal")]
    Interrupted,
    #[error("process timed out after {0:?}")]
    Timeout(Duration),
    #[error("process stdout exceeded the {0}-byte limit")]
    StdoutLimit(usize),
    #[error("process stderr exceeded the {0}-byte limit")]
    StderrLimit(usize),
    #[error("failed to spawn process")]
    Spawn(#[source] std::io::Error),
    #[error("failed while capturing process output")]
    Io(#[source] std::io::Error),
    #[error("process identifier cannot be represented as a process group")]
    InvalidPid,
    #[error("process stdin payload exceeds the 4096-byte setup limit")]
    InputLimit,
}

/// Run a setup child with inherited output, closed or bounded input, and a
/// process-group deadline. This keeps package and extraction progress visible.
pub(crate) fn run_status_bounded(
    command: &mut Command,
    deadline: Duration,
    input: Option<&[u8]>,
) -> Result<ExitStatus, CaptureError> {
    let _signals = crate::signal::install_handlers();
    run_status_bounded_with_shutdown(command, deadline, input, crate::signal::shutdown_requested)
}

fn run_status_bounded_with_shutdown(
    command: &mut Command,
    deadline: Duration,
    input: Option<&[u8]>,
    shutdown_requested: impl Fn() -> bool,
) -> Result<ExitStatus, CaptureError> {
    if input.is_some_and(|bytes| bytes.len() > 4096) {
        return Err(CaptureError::InputLimit);
    }
    command.process_group(0);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().map_err(CaptureError::Spawn)?;
    let Ok(pid) = i32::try_from(child.id()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CaptureError::InvalidPid);
    };
    if let Some(bytes) = input {
        let Some(mut stdin) = child.stdin.take() else {
            kill_group(pid, &mut child);
            let _ = child.wait();
            return Err(CaptureError::Io(std::io::Error::other(
                "child stdin pipe was not created",
            )));
        };
        if let Err(error) = stdin.write_all(bytes) {
            kill_group(pid, &mut child);
            let _ = child.wait();
            return Err(CaptureError::Io(error));
        }
    }
    let started = Instant::now();
    loop {
        if shutdown_requested() {
            terminate_group(pid, &mut child);
            return Err(CaptureError::Interrupted);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                kill_group(pid, &mut child);
                let _ = child.wait();
                return Err(CaptureError::Io(error));
            }
        }
        if started.elapsed() >= deadline {
            terminate_group(pid, &mut child);
            return Err(CaptureError::Timeout(deadline));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn terminate_group(pid: i32, child: &mut std::process::Child) {
    // Give wrappers such as sudo time to forward a catchable termination
    // signal to a privileged child before the wrapper is killed.
    let _ = unsafe { libc::kill(-pid, libc::SIGTERM) }; // SAFETY: pid is our positive group leader.
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if child.try_wait().is_ok_and(|status| status.is_some()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    kill_group(pid, child);
    let _ = child.wait();
}

pub(crate) fn run_bounded(
    command: &mut Command,
    policy: CapturePolicy,
) -> Result<BoundedOutput, CaptureError> {
    let _signals = crate::signal::install_handlers();
    run_bounded_with_shutdown(command, policy, crate::signal::shutdown_requested)
}

fn run_bounded_with_shutdown(
    command: &mut Command,
    policy: CapturePolicy,
    shutdown_requested: impl Fn() -> bool,
) -> Result<BoundedOutput, CaptureError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(CaptureError::Spawn)?;
    let Ok(pid) = i32::try_from(child.id()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CaptureError::InvalidPid);
    };
    let Some(mut stdout) = child.stdout.take() else {
        kill_group(pid, &mut child);
        let _ = child.wait();
        return Err(CaptureError::Io(std::io::Error::other(
            "child stdout pipe was not created",
        )));
    };
    let Some(mut stderr) = child.stderr.take() else {
        kill_group(pid, &mut child);
        drop(stdout);
        let _ = child.wait();
        return Err(CaptureError::Io(std::io::Error::other(
            "child stderr pipe was not created",
        )));
    };
    if let Err(error) =
        set_nonblocking(stdout.as_raw_fd()).and_then(|()| set_nonblocking(stderr.as_raw_fd()))
    {
        kill_group(pid, &mut child);
        let _ = child.wait();
        return Err(CaptureError::Io(error));
    }
    let mut stdout_bytes = Vec::with_capacity(policy.stdout_max.min(8192));
    let mut stderr_bytes = Vec::with_capacity(policy.stderr_max.min(8192));
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let started = Instant::now();
    let mut status = None;
    let failure = loop {
        match drain_available(&mut stdout, &mut stdout_bytes, policy.stdout_max) {
            Ok(Drain::Eof) => stdout_eof = true,
            Ok(Drain::Pending) => {}
            Ok(Drain::Overflow) => {
                break Some(CaptureError::StdoutLimit(policy.stdout_max));
            }
            Err(error) => break Some(CaptureError::Io(error)),
        }
        match drain_available(&mut stderr, &mut stderr_bytes, policy.stderr_max) {
            Ok(Drain::Eof) => stderr_eof = true,
            Ok(Drain::Pending) => {}
            Ok(Drain::Overflow) => {
                break Some(CaptureError::StderrLimit(policy.stderr_max));
            }
            Err(error) => break Some(CaptureError::Io(error)),
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => break Some(CaptureError::Io(error)),
            }
        }
        if shutdown_requested() {
            break Some(CaptureError::Interrupted);
        }
        if status.is_some() && stdout_eof && stderr_eof {
            break None;
        }
        if let CompletionPolicy::Deadline(timeout) = policy.completion
            && started.elapsed() >= timeout
        {
            break Some(CaptureError::Timeout(timeout));
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    if let Some(error) = failure {
        kill_group(pid, &mut child);
        drop(stdout);
        drop(stderr);
        let _ = child.wait();
        return Err(error);
    }

    let status = match status {
        Some(status) => status,
        None => child.wait().map_err(CaptureError::Io)?,
    };
    Ok(BoundedOutput {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Drain {
    Pending,
    Eof,
    Overflow,
}

fn drain_available(
    stream: &mut impl std::io::Read,
    captured: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<Drain> {
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(Drain::Eof),
            Ok(read) => {
                let remaining = limit.saturating_sub(captured.len());
                captured.extend_from_slice(&chunk[..read.min(remaining)]);
                if read > remaining {
                    return Ok(Drain::Overflow);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(Drain::Pending);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: F_GETFL/F_SETFL operate on the live pipe descriptor and do not
    // dereference pointers. The original flags are preserved.
    let flags = require_fcntl_success(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    // SAFETY: same live descriptor; O_NONBLOCK is valid for pipe read ends.
    require_fcntl_success(unsafe { libc::fcntl(fd, libc::F_SETFL, with_nonblocking(flags)) })?;
    Ok(())
}

fn with_nonblocking(flags: i32) -> i32 {
    flags | libc::O_NONBLOCK
}

fn require_fcntl_success(result: i32) -> std::io::Result<i32> {
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn kill_group(pid: i32, child: &mut std::process::Child) {
    // SAFETY: `pid` is the positive direct-child PID and process_group(0)
    // made it the leader of a new group. A negative PID targets that group.
    let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
    // Always target the direct child as well. This keeps cleanup correct if
    // the group signal fails or the child later changed its own group.
    let _ = child.kill();
}

#[cfg(test)]
#[expect(clippy::panic, clippy::unwrap_used, reason = "process fixtures")]
mod tests {
    use super::*;

    #[test]
    fn status_runner_distinguishes_exit_timeout_and_cancellation() {
        let success = run_status_bounded_with_shutdown(
            &mut Command::new("/bin/true"),
            Duration::from_secs(1),
            None,
            || false,
        )
        .unwrap();
        assert!(success.success());

        let exit = run_status_bounded_with_shutdown(
            &mut Command::new("/bin/false"),
            Duration::from_secs(1),
            None,
            || false,
        )
        .unwrap();
        assert_eq!(exit.code(), Some(1));

        let timeout = run_status_bounded_with_shutdown(
            Command::new("/bin/sleep").arg("2"),
            Duration::from_millis(20),
            None,
            || false,
        )
        .unwrap_err();
        assert!(matches!(timeout, CaptureError::Timeout(_)));

        let cancelled = run_status_bounded_with_shutdown(
            Command::new("/bin/sleep").arg("2"),
            Duration::from_secs(1),
            None,
            || true,
        )
        .unwrap_err();
        assert!(matches!(cancelled, CaptureError::Interrupted));
    }

    #[test]
    fn status_runner_closes_bounded_stdin_after_the_payload() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("payload");
        let status = run_status_bounded_with_shutdown(
            Command::new("/usr/bin/tee")
                .arg(&target)
                .stdout(Stdio::null()),
            Duration::from_secs(1),
            Some(b"fixed payload"),
            || false,
        )
        .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read(&target).unwrap(), b"fixed payload");
        let too_long = run_status_bounded_with_shutdown(
            &mut Command::new("/bin/true"),
            Duration::from_secs(1),
            Some(&[0; 4097]),
            || false,
        )
        .unwrap_err();
        assert!(matches!(too_long, CaptureError::InputLimit));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn status_runner_cancels_a_privileged_sudo_child_when_available() {
        if !Command::new("/usr/bin/sudo")
            .args(["-n", "/bin/true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let marker = fixture.path().join("child-pid");
        let script = format!("echo $$ > {}; exec /bin/sleep 30", marker.display());
        let error = run_status_bounded_with_shutdown(
            Command::new("/usr/bin/sudo")
                .args(["-n", "/bin/sh", "-c", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            Duration::from_secs(5),
            None,
            || marker.exists(),
        )
        .unwrap_err();
        assert!(matches!(error, CaptureError::Interrupted));
        let pid: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut gone = false;
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(gone, "sudo's privileged child remained after cancellation");
    }

    struct InterruptedThenData(u8);

    impl std::io::Read for InterruptedThenData {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            match self.0 {
                0 => {
                    self.0 = 1;
                    Err(std::io::ErrorKind::Interrupted.into())
                }
                1 => {
                    self.0 = 2;
                    buffer[0] = b'x';
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    struct PermissionDenied;

    impl std::io::Read for PermissionDenied {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
    }

    fn policy(limit: usize) -> CapturePolicy {
        CapturePolicy {
            stdout_max: limit,
            stderr_max: limit,
            completion: CompletionPolicy::Deadline(Duration::from_secs(2)),
        }
    }

    fn until_exit_policy(limit: usize) -> CapturePolicy {
        CapturePolicy {
            stdout_max: limit,
            stderr_max: limit,
            completion: CompletionPolicy::UntilExit,
        }
    }

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    fn assert_pid_gone(pid: i32) {
        for _ in 0..100 {
            // SAFETY: signal 0 only probes a PID written by this test's fixture.
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("fixture process {pid} still exists after cleanup");
    }

    #[test]
    fn concurrently_drains_both_streams_at_the_exact_limit() {
        let mut command = shell("(head -c 65536 /dev/zero >&2) & head -c 65536 /dev/zero; wait");
        let output = run_bounded(&mut command, policy(65_536)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 65_536);
        assert_eq!(output.stderr.len(), 65_536);
    }

    #[test]
    fn rejects_each_stream_one_byte_over_its_limit() {
        for _ in 0..100 {
            let mut stdout = shell("head -c 65 /dev/zero");
            assert!(matches!(
                run_bounded(&mut stdout, policy(64)),
                Err(CaptureError::StdoutLimit(64))
            ));
        }

        let mut stderr = shell("head -c 65 /dev/zero >&2");
        assert!(matches!(
            run_bounded(&mut stderr, policy(64)),
            Err(CaptureError::StderrLimit(64))
        ));
    }

    #[test]
    fn timeout_kills_a_descendant_that_keeps_the_pipes_open() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let pid_file = temp.path().join("descendant-pid");
        let mut command = shell(&format!(
            "sleep 30 & descendant=$!; echo \"$descendant\" > {}; wait",
            crate::shell::shell_escape(&pid_file.to_string_lossy())
        ));
        let started = Instant::now();
        let error = run_bounded(
            &mut command,
            CapturePolicy {
                stdout_max: 64,
                stderr_max: 64,
                completion: CompletionPolicy::Deadline(Duration::from_secs(3)),
            },
        )
        .unwrap_err();
        assert!(matches!(error, CaptureError::Timeout(_)));
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_pid_gone(pid);
    }

    #[test]
    fn completion_waits_for_each_pipe_independently() {
        for script in [
            "(exec 1>&-; sleep 0.2) & exec 2>&-",
            "(exec 2>&-; sleep 0.2) & exec 1>&-",
        ] {
            let mut command = shell(script);
            let started = Instant::now();
            let output = run_bounded(&mut command, policy(64)).unwrap();
            assert!(output.status.success());
            assert!(started.elapsed() >= Duration::from_millis(150));
        }
    }

    #[test]
    fn until_exit_allows_user_mediated_delay() {
        let mut command = shell("sleep 0.1; printf credential");
        let output = run_bounded(&mut command, until_exit_policy(64)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"credential");
    }

    #[test]
    fn interruption_kills_the_group_and_reaps_the_direct_child() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let pid_file = temp.path().join("descendant-pid");
        let mut command = shell(&format!(
            "sleep 30 & descendant=$!; echo \"$descendant\" > {}; wait",
            crate::shell::shell_escape(&pid_file.to_string_lossy())
        ));
        let started = Instant::now();
        let error = run_bounded_with_shutdown(&mut command, until_exit_policy(64), || {
            pid_file.exists() && started.elapsed() >= Duration::from_millis(100)
        })
        .unwrap_err();
        assert!(matches!(error, CaptureError::Interrupted));
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_pid_gone(pid);
    }

    #[test]
    fn signal_interrupts_until_exit_and_cleans_up_the_process_group() {
        const HELPER: &str = "COOP_CAPTURE_SIGNAL_HELPER";
        const PID_FILE: &str = "COOP_CAPTURE_SIGNAL_PID_FILE";
        if std::env::var_os(HELPER).is_some() {
            let pid_file = std::path::PathBuf::from(std::env::var_os(PID_FILE).unwrap());
            let mut command = shell(&format!(
                "sleep 30 & descendant=$!; echo \"$descendant\" > {}; wait",
                crate::shell::shell_escape(&pid_file.to_string_lossy())
            ));
            let error = run_bounded(&mut command, until_exit_policy(64)).unwrap_err();
            assert!(matches!(error, CaptureError::Interrupted));
            let pid = std::fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_pid_gone(pid);
            return;
        }

        let temp = crate::test_support::private_tempdir().unwrap();
        let pid_file = temp.path().join("descendant-pid");
        let executable = std::env::current_exe().unwrap();
        let mut helper = Command::new(executable)
            .args([
                "--exact",
                "process_output::tests::signal_interrupts_until_exit_and_cleans_up_the_process_group",
                "--nocapture",
            ])
            .env(HELPER, "1")
            .env(PID_FILE, &pid_file)
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if pid_file.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(pid_file.exists(), "helper did not start its process group");
        let helper_pid = i32::try_from(helper.id()).unwrap();
        // SAFETY: signal only the test helper PID spawned immediately above.
        assert_eq!(unsafe { libc::kill(helper_pid, libc::SIGTERM) }, 0);
        assert!(helper.wait().unwrap().success());
    }

    #[test]
    fn timeout_reaps_the_direct_child() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let pid_file = temp.path().join("pid");
        let mut command = shell(&format!(
            "echo $$ > {}; sleep 30",
            crate::shell::shell_escape(&pid_file.to_string_lossy())
        ));
        let error = run_bounded(
            &mut command,
            CapturePolicy {
                stdout_max: 64,
                stderr_max: 64,
                completion: CompletionPolicy::Deadline(Duration::from_secs(3)),
            },
        )
        .unwrap_err();
        assert!(matches!(error, CaptureError::Timeout(_)));
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_pid_gone(pid);
    }

    #[test]
    fn detached_descendant_cannot_extend_the_return_deadline() {
        const HELPER: &str = "COOP_CAPTURE_SETSID_HELPER";
        const PID_FILE: &str = "COOP_CAPTURE_SETSID_PID_FILE";
        if std::env::var_os(HELPER).is_some() {
            // SAFETY: the nested fixture is not a process-group leader and
            // intentionally detaches to model a hostile resolver descendant.
            assert!(unsafe { libc::setsid() } > 0);
            std::fs::write(
                std::env::var_os(PID_FILE).unwrap(),
                std::process::id().to_string(),
            )
            .unwrap();
            std::thread::sleep(Duration::from_secs(30));
            return;
        }

        let temp = crate::test_support::private_tempdir().unwrap();
        let pid_file = temp.path().join("detached-pid");
        let executable = std::env::current_exe().unwrap();
        let script = format!(
            "{} --exact process_output::tests::detached_descendant_cannot_extend_the_return_deadline --nocapture &",
            crate::shell::shell_escape(&executable.to_string_lossy())
        );
        let mut command = shell(&script);
        command.env(HELPER, "1").env(PID_FILE, &pid_file);
        let started = Instant::now();
        let error = run_bounded(
            &mut command,
            CapturePolicy {
                stdout_max: 1024,
                stderr_max: 1024,
                completion: CompletionPolicy::Deadline(Duration::from_secs(3)),
            },
        )
        .unwrap_err();
        assert!(matches!(error, CaptureError::Timeout(_)));
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: terminate only the PID written by this test's detached fixture.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    #[test]
    fn preserves_nonzero_status_and_invalid_utf8_as_bounded_bytes() {
        let mut command = shell("printf '\\377' >&1; printf diagnostic >&2; exit 7");
        let output = run_bounded(&mut command, policy(64)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, [0xff]);
        assert_eq!(output.stderr, b"diagnostic");
    }

    #[test]
    fn drain_retries_interrupts_and_preserves_other_io_errors() {
        let mut captured = Vec::new();
        assert_eq!(
            drain_available(&mut InterruptedThenData(0), &mut captured, 8).unwrap(),
            Drain::Eof
        );
        assert_eq!(captured, b"x");

        let error = drain_available(&mut PermissionDenied, &mut Vec::new(), 8).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn fcntl_result_rejects_only_the_error_sentinel() {
        assert_eq!(require_fcntl_success(0).unwrap(), 0);
        assert_eq!(require_fcntl_success(1).unwrap(), 1);
        assert!(require_fcntl_success(-1).is_err());
    }

    #[test]
    fn adding_nonblocking_preserves_existing_flags() {
        assert_eq!(with_nonblocking(0), libc::O_NONBLOCK);
        assert_eq!(
            with_nonblocking(libc::O_APPEND),
            libc::O_APPEND | libc::O_NONBLOCK
        );
        assert_eq!(
            with_nonblocking(libc::O_APPEND | libc::O_NONBLOCK),
            libc::O_APPEND | libc::O_NONBLOCK
        );
    }
}
