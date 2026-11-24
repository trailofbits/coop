//! Identity-bound lifecycle state for long-running host helper processes.
//!
//! A numeric PID is not durable authority: after a helper exits the kernel can
//! reuse it for an unrelated process. This module persists enough native
//! identity to reject reuse before signaling, and removes state only after the
//! complete process group is confirmed absent.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STATE_SCHEMA: u32 = 1;
const STATE_MAX_BYTES: usize = 16 * 1024;
const TERM_GRACE: Duration = Duration::from_secs(2);
const KILL_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HelperRole {
    CredentialProxy,
    ReverseTunnel,
}

impl std::fmt::Display for HelperRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CredentialProxy => formatter.write_str("credential proxy"),
            Self::ReverseTunnel => formatter.write_str("credential proxy reverse tunnel"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ProcessIdentity {
    pid: i32,
    pgid: i32,
    uid: u32,
    start_id: String,
    executable_hex: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct HelperState {
    schema: u32,
    role: HelperRole,
    provider: String,
    #[serde(flatten)]
    identity: ProcessIdentity,
    created_unix_secs: u64,
}

/// Observe `pid` after it has reached its serving state and persist the exact
/// process identity in an owner-only atomic state record.
pub(crate) fn write_state(path: &Path, role: HelperRole, provider: &str, pid: u32) -> Result<()> {
    let pid = i32::try_from(pid).context("helper PID exceeds the platform PID range")?;
    let identity =
        observe(pid).with_context(|| format!("Failed to observe {role} process {pid}"))?;
    anyhow::ensure!(
        identity.pgid == pid,
        "Refusing to record {role} process {pid}: expected process group {pid}, observed {}",
        identity.pgid
    );
    let state = HelperState {
        schema: STATE_SCHEMA,
        role,
        provider: provider.to_string(),
        identity,
        created_unix_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("System clock is before the Unix epoch")?
            .as_secs(),
    };
    let mut json = serde_json::to_string_pretty(&state).context("Failed to encode helper state")?;
    json.push('\n');
    anyhow::ensure!(
        json.len() <= STATE_MAX_BYTES,
        "Encoded helper state exceeds {STATE_MAX_BYTES} bytes"
    );
    crate::fs_util::atomic_write_with_mode(path, &json, 0o600)
        .with_context(|| format!("Failed to write helper state {}", path.display()))
}

/// Require a saved helper to still be the exact live process that was
/// recorded. This is a readiness check only and never signals.
pub(crate) fn ensure_live(path: &Path, role: HelperRole, provider: &str) -> Result<()> {
    let state = load_state(path, role, provider)?;
    require_match(&state).with_context(|| {
        format!(
            "Saved {role} identity at {} no longer matches the live process",
            path.display()
        )
    })?;
    Ok(())
}

/// Stop one saved helper process group. Missing state is already stopped;
/// malformed, legacy, or mismatched state is retained and never signaled.
pub(crate) fn stop(path: &Path, role: HelperRole, provider: &str) -> Result<()> {
    stop_with_grace(path, role, provider, TERM_GRACE, KILL_GRACE)
}

fn stop_with_grace(
    path: &Path,
    role: HelperRole,
    provider: &str,
    term_grace: Duration,
    kill_grace: Duration,
) -> Result<()> {
    let Some(state) = load_optional_state(path, role, provider)? else {
        return Ok(());
    };

    match observe(state.identity.pid) {
        Ok(identity) => require_identity(&state.identity, &identity).with_context(|| {
            format!(
                "Refusing to signal {role}: saved identity at {} does not match PID {}",
                path.display(),
                state.identity.pid
            )
        })?,
        Err(error) => match classify_observation_failure(&error) {
            ObservationFailure::Absent => {
                bail!(
                    "{role} leader {} is absent; without a stable authority coop cannot prove process group {} absent. State retained at {} for manual recovery",
                    state.identity.pid,
                    state.identity.pgid,
                    path.display()
                );
            }
            ObservationFailure::Unverifiable => {
                return Err(error).with_context(|| {
                    format!(
                        "Unable to verify {role} identity at {}; signal nothing and inspect the retained state",
                        path.display()
                    )
                });
            }
        },
    }

    let stable = StableProcess::open(state.identity.pid)?;
    signal_verified_group(&state, &stable, libc::SIGTERM)?;
    if wait_for_group_absence(&state, &stable, term_grace)? {
        remove_state(path)?;
        return Ok(());
    }

    // The pidfd keeps the original process-group authority even if its leader
    // exits during the grace period. No numeric PID/PGID is re-resolved here.
    stable.send_group_signal(libc::SIGKILL)?;
    anyhow::ensure!(
        wait_for_group_absence(&state, &stable, kill_grace)?,
        "{role} process group {} remained after SIGKILL; state retained at {}",
        state.identity.pgid,
        path.display()
    );
    remove_state(path)
}

fn load_state(path: &Path, role: HelperRole, provider: &str) -> Result<HelperState> {
    load_optional_state(path, role, provider)?
        .with_context(|| format!("{role} state is missing at {}", path.display()))
}

fn load_optional_state(
    path: &Path,
    role: HelperRole,
    provider: &str,
) -> Result<Option<HelperState>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to open helper state {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("Failed to inspect helper state {}", path.display()))?;
    // SAFETY: geteuid has no preconditions and does not access memory.
    let current_uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        metadata.is_file() && metadata.uid() == current_uid,
        "Helper state {} must be a regular file owned by the current user",
        path.display()
    );

    let mut bytes = Vec::with_capacity(1024);
    file.take((STATE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read helper state {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= STATE_MAX_BYTES,
        "Helper state {} exceeds {STATE_MAX_BYTES} bytes",
        path.display()
    );
    if std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
        .is_some()
    {
        bail!(
            "Legacy plain-PID helper state at {} is not signal authority; inspect the process manually, then remove the state file. --force must not bypass identity verification",
            path.display()
        );
    }
    anyhow::ensure!(
        metadata.permissions().mode().trailing_zeros() >= 6,
        "Helper state {} must be owner-only; signal nothing",
        path.display()
    );
    let state: HelperState = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "Malformed helper state at {}; signal nothing and inspect or remove it manually",
            path.display()
        )
    })?;
    anyhow::ensure!(
        state.schema == STATE_SCHEMA,
        "Unsupported helper-state schema {} at {}; signal nothing",
        state.schema,
        path.display()
    );
    anyhow::ensure!(
        state.role == role && state.provider == provider,
        "Helper state role/provider mismatch at {}; signal nothing",
        path.display()
    );
    validate_saved_identity(&state.identity, path)?;
    Ok(Some(state))
}

fn validate_saved_identity(identity: &ProcessIdentity, path: &Path) -> Result<()> {
    anyhow::ensure!(
        identity.pid > 0
            && identity.pgid == identity.pid
            && !identity.start_id.is_empty()
            && !identity.executable_hex.is_empty(),
        "Invalid helper identity at {}; signal nothing",
        path.display()
    );
    Ok(())
}

fn require_match(state: &HelperState) -> Result<ProcessIdentity> {
    let observed = observe(state.identity.pid)?;
    require_identity(&state.identity, &observed)?;
    Ok(observed)
}

fn require_identity(expected: &ProcessIdentity, observed: &ProcessIdentity) -> Result<()> {
    anyhow::ensure!(expected == observed, "process identity mismatch");
    Ok(())
}

fn remove_state(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to remove helper state {}", path.display()))
        }
    }
}

fn wait_for_group_absence(
    state: &HelperState,
    stable: &StableProcess,
    grace: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + grace;
    loop {
        // The helper may still be a child of this coop invocation during
        // startup rollback. Reap it opportunistically so a dead leader zombie
        // does not make its process group appear live until coop itself exits.
        let _ = reap_if_child(state.identity.pid)?;
        if !stable.group_exists()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn reap_if_child(pid: i32) -> Result<bool> {
    let mut status = 0;
    // SAFETY: WNOHANG observes/reaps only the saved positive PID when it is a
    // child of this coop invocation. ECHILD is expected across invocations.
    let result = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
    match result {
        1.. => Ok(true),
        0 => Ok(false),
        _ => {
            let error = std::io::Error::last_os_error();
            match classify_reap_error(&error) {
                ReapFailure::NoChild => Ok(true),
                ReapFailure::Failed => Err(error).context("Failed to reap helper process"),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReapFailure {
    NoChild,
    Failed,
}

fn classify_reap_error(error: &std::io::Error) -> ReapFailure {
    if error.raw_os_error() == Some(libc::ECHILD) {
        ReapFailure::NoChild
    } else {
        ReapFailure::Failed
    }
}

fn signal_verified_group(state: &HelperState, stable: &StableProcess, signal: i32) -> Result<()> {
    require_match(state).context("Helper identity changed immediately before signaling")?;
    stable.send_group_signal(signal)
}

struct StableProcess {
    #[cfg(target_os = "linux")]
    pidfd: std::os::fd::OwnedFd,
}

impl StableProcess {
    fn open(pid: i32) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            // SAFETY: pidfd_open takes a positive observed PID and zero flags.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            let pidfd = if fd >= 0 {
                let fd = i32::try_from(fd).context("pidfd exceeds descriptor range")?;
                // SAFETY: successful pidfd_open returned a new owned descriptor.
                unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }
            } else {
                let error = std::io::Error::last_os_error();
                let context = match error.raw_os_error() {
                    Some(libc::ENOSYS | libc::EINVAL) => {
                        "This Linux kernel does not support stable helper signal authority; signal nothing and clean up manually"
                    }
                    _ => {
                        "Failed to acquire stable helper signal authority; signal nothing and preserve the retained state"
                    }
                };
                return Err(error).context(context);
            };
            Ok(Self { pidfd })
        }
        #[cfg(target_os = "macos")]
        {
            let _ = pid;
            bail!(
                "macOS cannot provide race-free helper signal authority; signal nothing and clean up the retained helper state manually"
            )
        }
    }

    /// Signal the saved process group through a kernel-stable authority.
    fn send_group_signal(&self, signal: i32) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // Linux uapi pidfd.h: THREAD=bit 0, THREAD_GROUP=bit 1, and the
            // POSIX process-group selector used here is bit 2.
            const PIDFD_SIGNAL_PROCESS_GROUP: u32 = 1 << 2;
            // SAFETY: pidfd is owned and live; siginfo is null and the group
            // flag atomically binds the signal to its original process group.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    PIDFD_SIGNAL_PROCESS_GROUP,
                )
            };
            if result == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => Ok(()),
                _ => Err(error).context(
                    "Stable process-group signaling is unavailable; signal nothing and clean up manually",
                ),
            }
        }
        #[cfg(target_os = "macos")]
        {
            let _ = self;
            let _ = signal;
            bail!("macOS has no stable process-group signaling authority")
        }
    }

    fn group_exists(&self) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            const PIDFD_SIGNAL_PROCESS_GROUP: u32 = 1 << 2;
            // Signal zero performs an existence/permission probe through the
            // same kernel-stable process-group authority used for teardown.
            // SAFETY: pidfd is owned and live; siginfo is null.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    0,
                    std::ptr::null::<libc::siginfo_t>(),
                    PIDFD_SIGNAL_PROCESS_GROUP,
                )
            };
            if result == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => Ok(false),
                _ => Err(error).context("Failed to probe stable helper process group"),
            }
        }
        #[cfg(target_os = "macos")]
        {
            let _ = self;
            bail!("macOS has no stable process-group probing authority")
        }
    }
}

#[cfg(target_os = "linux")]
fn observe(pid: i32) -> Result<ProcessIdentity> {
    use std::os::unix::ffi::OsStrExt;

    let proc_dir = std::path::PathBuf::from(format!("/proc/{pid}"));
    let metadata =
        fs::metadata(&proc_dir).with_context(|| format!("Failed to stat /proc/{pid}"))?;
    let stat = fs::read_to_string(proc_dir.join("stat"))
        .with_context(|| format!("Failed to read /proc/{pid}/stat"))?;
    let (parsed_pid, _, pgid, start_ticks) = parse_linux_stat(&stat)?;
    anyhow::ensure!(parsed_pid == pid, "Observed /proc PID changed");
    let executable = fs::read_link(proc_dir.join("exe"))
        .with_context(|| format!("Failed to read /proc/{pid}/exe"))?;
    Ok(ProcessIdentity {
        pid,
        pgid,
        uid: metadata.uid(),
        start_id: format!("linux:{start_ticks}"),
        executable_hex: hex::encode(executable.as_os_str().as_bytes()),
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_stat(stat: &str) -> Result<(i32, char, i32, u64)> {
    let open = stat
        .find('(')
        .context("Linux process stat is missing comm start")?;
    let close = stat
        .rfind(')')
        .context("Linux process stat is missing comm end")?;
    anyhow::ensure!(close > open, "Linux process stat has malformed comm");
    let pid = stat[..open]
        .trim()
        .parse::<i32>()
        .context("Linux process stat has malformed PID")?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    anyhow::ensure!(fields.len() > 19, "Linux process stat is truncated");
    let state = fields[0]
        .chars()
        .next()
        .context("Linux process stat has malformed state")?;
    let pgid = fields[2]
        .parse::<i32>()
        .context("Linux process stat has malformed PGID")?;
    let start_ticks = fields[19]
        .parse::<u64>()
        .context("Linux process stat has malformed start time")?;
    Ok((pid, state, pgid, start_ticks))
}

#[cfg(target_os = "macos")]
fn observe(pid: i32) -> Result<ProcessIdentity> {
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
        .context("proc_bsdinfo size exceeds c_int")?;
    // SAFETY: proc_bsdinfo is a plain C output struct initialized before use.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: buffer and size describe `info`; pid is positive at call sites.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if written != size {
        let error = std::io::Error::last_os_error();
        return Err(if error.raw_os_error().is_some() {
            error
        } else {
            std::io::Error::new(std::io::ErrorKind::NotFound, "process identity unavailable")
        })
        .with_context(|| format!("Failed to query process {pid}"));
    }
    anyhow::ensure!(
        info.pbi_pid == pid.cast_unsigned(),
        "Observed process PID changed"
    );

    let mut path = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: path is a writable buffer of the supplied size.
    let path_len = unsafe {
        libc::proc_pidpath(
            pid,
            path.as_mut_ptr().cast(),
            u32::try_from(path.len()).context("process path buffer exceeds u32")?,
        )
    };
    if path_len <= 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to query executable path for process {pid}"));
    }
    path.truncate(usize::try_from(path_len).context("negative process path length")?);
    Ok(ProcessIdentity {
        pid,
        pgid: i32::try_from(info.pbi_pgid).context("process group exceeds i32")?,
        uid: info.pbi_uid,
        start_id: format!("macos:{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
        executable_hex: hex::encode(path),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservationFailure {
    Absent,
    Unverifiable,
}

fn classify_observation_failure(error: &anyhow::Error) -> ObservationFailure {
    if error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            io.kind() == std::io::ErrorKind::NotFound || io.raw_os_error() == Some(libc::ESRCH)
        })
    }) {
        ObservationFailure::Absent
    } else {
        ObservationFailure::Unverifiable
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "native process fixtures")]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    fn spawn_group(script: &str) -> std::process::Child {
        let mut command = Command::new("sh");
        command.args(["-c", script]).process_group(0);
        command.spawn().unwrap()
    }

    fn state_path(temp: &tempfile::TempDir) -> std::path::PathBuf {
        temp.path().join("helper.pid")
    }

    #[test]
    fn parses_linux_stat_with_spaces_and_parentheses_in_comm() {
        let mut fields = vec!["S"; 20];
        fields[2] = "41";
        fields[19] = "987654";
        let stat = format!("41 (name with ) paren) {}", fields.join(" "));
        assert_eq!(parse_linux_stat(&stat).unwrap(), (41, 'S', 41, 987_654));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn matching_group_is_stopped_and_state_removed() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("sleep 30 & wait");
        let pid = i32::try_from(child.id()).unwrap();
        write_state(&path, HelperRole::CredentialProxy, "test", child.id()).unwrap();
        ensure_live(&path, HelperRole::CredentialProxy, "test").unwrap();
        stop_with_grace(
            &path,
            HelperRole::CredentialProxy,
            "test",
            Duration::from_secs(2),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(!path.exists());
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert_eq!(child.wait().unwrap_err().raw_os_error(), Some(libc::ECHILD));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn term_ignoring_leader_is_killed_after_grace() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("trap '' TERM; sleep 30 & wait");
        let pid = i32::try_from(child.id()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        write_state(&path, HelperRole::ReverseTunnel, "test", child.id()).unwrap();
        stop_with_grace(
            &path,
            HelperRole::ReverseTunnel,
            "test",
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(!path.exists());
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert_eq!(child.wait().unwrap_err().raw_os_error(), Some(libc::ECHILD));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_pidfd_kills_descendant_after_leader_exits_on_term() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let ready = temp.path().join("descendant-ready");
        let leader_gone = temp.path().join("leader-gone");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "trap 'exit 0' TERM; sh -c 'trap \"\" TERM; : > \"$1\"; parent=$PPID; while kill -0 \"$parent\" 2>/dev/null; do sleep 0.01; done; : > \"$2\"; while :; do sleep 1; done' sh \"$1\" \"$2\" & while :; do sleep 1; done",
                "coop-test",
                ready.to_str().unwrap(),
                leader_gone.to_str().unwrap(),
            ])
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        write_state(&path, HelperRole::CredentialProxy, "test", child.id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "descendant did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }

        stop_with_grace(
            &path,
            HelperRole::CredentialProxy,
            "test",
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(leader_gone.exists(), "leader did not exit before SIGKILL");
        assert!(!path.exists());
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(child.wait().unwrap_err().raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    fn mismatched_start_identity_is_retained_and_not_signaled() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("exec sleep 30");
        write_state(&path, HelperRole::CredentialProxy, "test", child.id()).unwrap();
        let mut state: HelperState = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state.identity.start_id.push_str("-recycled");
        crate::fs_util::atomic_write_with_mode(
            &path,
            &serde_json::to_string(&state).unwrap(),
            0o600,
        )
        .unwrap();
        let error = stop_with_grace(
            &path,
            HelperRole::CredentialProxy,
            "test",
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("does not match"));
        assert!(path.exists());
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn identity_comparison_requires_every_observed_field() {
        let expected = observe(i32::try_from(std::process::id()).unwrap()).unwrap();
        let mut variants = Vec::new();
        let mut pid = expected.clone();
        pid.pid += 1;
        variants.push(pid);
        let mut pgid = expected.clone();
        pgid.pgid += 1;
        variants.push(pgid);
        let mut uid = expected.clone();
        uid.uid = uid.uid.wrapping_add(1);
        variants.push(uid);
        let mut start = expected.clone();
        start.start_id.push_str("-different");
        variants.push(start);
        let mut executable = expected.clone();
        executable.executable_hex.push_str("00");
        variants.push(executable);

        assert!(require_identity(&expected, &expected).is_ok());
        for observed in variants {
            assert!(require_identity(&expected, &observed).is_err());
        }
    }

    #[test]
    fn legacy_pid_is_retained_and_never_signaled() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("sleep 30");
        crate::fs_util::atomic_write_with_mode(&path, &child.id().to_string(), 0o600).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = stop(&path, HelperRole::CredentialProxy, "test").unwrap_err();
        assert!(format!("{error:#}").contains("Legacy plain-PID"));
        assert!(path.exists());
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn wrong_role_and_provider_are_retained() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("sleep 30");
        write_state(&path, HelperRole::CredentialProxy, "openai", child.id()).unwrap();
        for (role, provider) in [
            (HelperRole::ReverseTunnel, "openai"),
            (HelperRole::CredentialProxy, "anthropic"),
        ] {
            assert!(stop(&path, role, provider).is_err());
            assert!(child.try_wait().unwrap().is_none());
        }
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn malformed_state_is_retained() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        crate::fs_util::atomic_write_with_mode(&path, "{not-json", 0o600).unwrap();
        let error = stop(&path, HelperRole::CredentialProxy, "test").unwrap_err();
        assert!(format!("{error:#}").contains("Malformed helper state"));
        assert!(path.exists());
    }

    #[test]
    fn missing_unreadable_and_oversized_state_are_distinct() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let missing = ensure_live(&path, HelperRole::CredentialProxy, "test").unwrap_err();
        assert!(format!("{missing:#}").contains("state is missing"));

        crate::fs_util::atomic_write_with_mode(&path, "{}", 0o000).unwrap();
        let unreadable = ensure_live(&path, HelperRole::CredentialProxy, "test").unwrap_err();
        assert!(
            format!("{unreadable:#}").contains("Failed to open helper state")
                || format!("{unreadable:#}").contains("must be a regular owner-only file")
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        crate::fs_util::atomic_write_with_mode(&path, &"x".repeat(STATE_MAX_BYTES + 1), 0o600)
            .unwrap();
        let oversized = ensure_live(&path, HelperRole::CredentialProxy, "test").unwrap_err();
        let message = format!("{oversized:#}");
        assert!(
            message.contains(&format!("exceeds {STATE_MAX_BYTES} bytes")),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn invalid_saved_identity_fields_are_rejected_before_observation() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("exec sleep 30");
        write_state(&path, HelperRole::CredentialProxy, "test", child.id()).unwrap();
        let valid: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        for (field, value) in [
            ("pid", serde_json::json!(0)),
            ("pgid", serde_json::json!(1)),
            ("start_id", serde_json::json!("")),
            ("executable_hex", serde_json::json!("")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            crate::fs_util::atomic_write_with_mode(
                &path,
                &serde_json::to_string(&invalid).unwrap(),
                0o600,
            )
            .unwrap();
            let error = ensure_live(&path, HelperRole::CredentialProxy, "test").unwrap_err();
            assert!(format!("{error:#}").contains("Invalid helper identity"));
        }
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn absence_classification_requires_native_not_found() {
        assert_eq!(
            classify_observation_failure(&anyhow::Error::new(std::io::Error::from(
                std::io::ErrorKind::NotFound
            ))),
            ObservationFailure::Absent
        );
        assert_eq!(
            classify_observation_failure(&anyhow::Error::new(std::io::Error::from_raw_os_error(
                libc::ESRCH
            ))),
            ObservationFailure::Absent
        );
        assert_eq!(
            classify_observation_failure(&anyhow::Error::new(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))),
            ObservationFailure::Unverifiable
        );
        assert_eq!(
            classify_observation_failure(&anyhow::Error::msg("not found is only text")),
            ObservationFailure::Unverifiable
        );
    }

    #[test]
    fn reap_error_classifies_only_echild_as_settled() {
        assert_eq!(
            classify_reap_error(&std::io::Error::from_raw_os_error(libc::ECHILD)),
            ReapFailure::NoChild
        );
        assert_eq!(
            classify_reap_error(&std::io::Error::from_raw_os_error(libc::EINVAL)),
            ReapFailure::Failed
        );
    }

    #[test]
    fn reap_distinguishes_live_exited_and_non_child_processes() {
        let mut live = spawn_group("exec sleep 30");
        assert!(!reap_if_child(i32::try_from(live.id()).unwrap()).unwrap());
        live.kill().unwrap();
        live.wait().unwrap();

        let mut exited = spawn_group("exit 0");
        let exited_pid = i32::try_from(exited.id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reap_if_child(exited_pid).unwrap() {
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            exited.wait().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );

        assert!(reap_if_child(i32::try_from(std::process::id()).unwrap()).unwrap());
    }

    #[test]
    fn exited_leader_with_live_descendant_retains_state() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let ready = temp.path().join("descendant-ready");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sh -c 'trap \"\" TERM; : > \"$1\"; exec sleep 30' sh \"$1\" & wait",
                "coop-test",
                ready.to_str().unwrap(),
            ])
            .process_group(0);
        let mut child = command.spawn().unwrap();
        write_state(&path, HelperRole::CredentialProxy, "test", child.id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "descendant did not become ready");
            assert!(child.try_wait().unwrap().is_none(), "leader exited early");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Killing only the leader models a helper that exits while one of its
        // group descendants remains. The saved group must not become signal
        // authority on its own.
        child.kill().unwrap();
        child.wait().unwrap();
        let error = stop_with_grace(
            &path,
            HelperRole::CredentialProxy,
            "test",
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("process group"));
        assert!(path.exists());
        let pgid = i32::try_from(child.id()).unwrap();
        // SAFETY: cleanup targets only the test-created process group.
        let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }

    #[test]
    fn dead_process_retains_state_without_stable_group_authority() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("exec sleep 30");
        write_state(&path, HelperRole::CredentialProxy, "test", child.id()).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let error = stop(&path, HelperRole::CredentialProxy, "test").unwrap_err();
        assert!(format!("{error:#}").contains("cannot prove process group"));
        assert!(path.exists());
    }

    #[test]
    fn state_is_owner_only_and_bounded() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("sleep 30");
        write_state(&path, HelperRole::CredentialProxy, "test", child.id()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ensure_live(&path, HelperRole::CredentialProxy, "test").is_err());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pidfd_group_signal_preserves_identity() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("sleep 30");
        write_state(&path, HelperRole::CredentialProxy, "test", child.id()).unwrap();
        let state = load_state(&path, HelperRole::CredentialProxy, "test").unwrap();

        let stable = StableProcess::open(state.identity.pid).unwrap();
        signal_verified_group(&state, &stable, 0).unwrap();
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_refuses_to_signal_a_live_helper_without_stable_authority() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = state_path(&temp);
        let mut child = spawn_group("exec sleep 30");
        write_state(&path, HelperRole::CredentialProxy, "test", child.id()).unwrap();

        let error = stop(&path, HelperRole::CredentialProxy, "test").unwrap_err();
        assert!(format!("{error:#}").contains("race-free helper signal authority"));
        assert!(path.exists());
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
