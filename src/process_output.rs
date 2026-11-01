//! Bounded capture for short-lived host subprocesses.

use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CapturePolicy {
    pub(crate) stdout_max: usize,
    pub(crate) stderr_max: usize,
    pub(crate) timeout: Duration,
}

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

#[derive(Debug, Error)]
pub(crate) enum CaptureError {
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
}

pub(crate) fn run_bounded(
    command: &mut Command,
    policy: CapturePolicy,
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
    let mut stdout = child.stdout.take().ok_or_else(|| {
        CaptureError::Io(std::io::Error::other("child stdout pipe was not created"))
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        CaptureError::Io(std::io::Error::other("child stderr pipe was not created"))
    })?;
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
    let deadline = Instant::now() + policy.timeout;
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
        if status.is_some() && stdout_eof && stderr_eof {
            break None;
        }
        if Instant::now() >= deadline {
            break Some(CaptureError::Timeout(policy.timeout));
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
            timeout: Duration::from_secs(2),
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
                timeout: Duration::from_secs(3),
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
                timeout: Duration::from_secs(3),
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
                timeout: Duration::from_secs(3),
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
