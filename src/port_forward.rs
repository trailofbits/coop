//! Host-to-guest TCP port forwarding for the lifetime of a VM.
//!
//! Both backends use a backgrounded SSH session with `-L` forwards,
//! parked under a per-instance control socket so `teardown_ssh_forwards`
//! can tear it down cleanly with `ssh -O exit` (without touching the
//! user's other SSH sessions). Lima exposes a normal SSH target via
//! `limactl info`, so the same path works there.
//!
//! `check_host_port_collisions` runs before any state is created so an
//! in-use host port fails fast — the error names the offending port
//! and offers a copy-pasteable remediation flag.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::backend::SshTarget;
use crate::config::{Instance, PortForward};

/// Persisted forward set written at `start` so `restart` can re-apply
/// the same forwards without the user passing `--forward-port` again.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ForwardsState {
    #[serde(default)]
    pub forwards: Vec<PortForward>,
}

impl ForwardsState {
    pub fn save(&self, inst: &Instance) -> Result<()> {
        let path = inst.forwards_state_path();
        if self.forwards.is_empty() {
            // Don't litter the instance dir with empty state.
            if path.exists()
                && let Err(e) = fs::remove_file(&path)
            {
                tracing::debug!(
                    "Failed to remove empty forwards state {} (non-fatal): {e}",
                    path.display()
                );
            }
            return Ok(());
        }
        let json =
            serde_json::to_string_pretty(self).context("Failed to serialize forwards state")?;
        crate::fs_util::atomic_write_json(&path, &json).context("Failed to write forwards.json")?;
        tracing::debug!("Wrote forwards state to {}", path.display());
        Ok(())
    }

    pub fn try_load(inst: &Instance) -> Result<Option<Self>> {
        let path = inst.forwards_state_path();
        match fs::read_to_string(&path) {
            Ok(content) => {
                let state =
                    serde_json::from_str(&content).context("Failed to parse forwards.json")?;
                Ok(Some(state))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(anyhow::Error::new(e).context(format!("Failed to read {}", path.display())))
            }
        }
    }
}

/// Probe each host port; bail with an actionable message on the first
/// collision so we never partially apply a forward set.
///
/// Also flags duplicate host ports within `forwards` (two guests
/// fighting for the same host port).
pub fn check_host_port_collisions(forwards: &[PortForward]) -> Result<()> {
    let mut seen: HashSet<NonZeroU16> = HashSet::new();
    for f in forwards {
        if !seen.insert(f.host) {
            bail!(
                "Duplicate host port {host} in forward set — \
                 only one guest port can bind a given host port.\n\
                 Pick distinct hosts, e.g. `--forward-port G:{alt}`.",
                host = f.host,
                alt = next_suggestion(f.host),
            );
        }

        match TcpListener::bind(("127.0.0.1", f.host.get())) {
            Ok(listener) => {
                drop(listener);
            }
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                bail!(
                    "Host port {host} is already in use — \
                     cannot forward guest port {guest}.\n\
                     Pick another host port, e.g. \
                     `--forward-port {guest}:{alt}`.",
                    host = f.host,
                    guest = f.guest,
                    alt = next_suggestion(f.host),
                );
            }
            Err(e) => {
                bail!(
                    "Failed to probe host port {host}: {e}.\n\
                     Pick another host port, e.g. \
                     `--forward-port {guest}:{alt}`.",
                    host = f.host,
                    guest = f.guest,
                    alt = next_suggestion(f.host),
                );
            }
        }
    }
    Ok(())
}

/// Suggest the next port up, wrapping to `host - 1` when at `u16::MAX`.
fn next_suggestion(host: NonZeroU16) -> u16 {
    host.get().checked_add(1).unwrap_or(host.get() - 1)
}

/// Per-instance control socket for the port-forwarder SSH session.
fn forwards_control_path(inst: &Instance) -> PathBuf {
    inst.dir.join("forwards.sock")
}

/// Spawn a backgrounded SSH connection with `-L` forwards for every
/// entry in `forwards`. The connection is owned by a dedicated control
/// master at `<inst>/forwards.sock`, so `teardown_ssh_forwards` can
/// terminate it cleanly without touching the user's own SSH sessions.
///
/// No-op when `forwards` is empty.
///
/// Caller must have already run [`check_host_port_collisions`] so we
/// fail at allocation time rather than mid-bind.
pub fn spawn_ssh_forwards(
    inst: &Instance,
    target: &SshTarget,
    forwards: &[PortForward],
) -> Result<()> {
    if forwards.is_empty() {
        return Ok(());
    }

    // A previous run may have crashed without stop cleanup; tear down
    // any leftover master before binding so we don't race the old one
    // for the same host ports.
    teardown_ssh_forwards(inst, target);

    let control_path = forwards_control_path(inst);
    if let Some(parent) = control_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create instance dir {} for forward socket",
                parent.display()
            )
        })?;
    }

    let mut args = target.ssh_opts();
    // -f: fork to background after authentication. The parent exits 0
    //     and the child holds the forwards.
    // -N: no remote command — pure tunneling.
    // -T: no PTY.
    // ControlMaster=yes / ControlPath / ControlPersist=yes: park the
    //     connection under a known socket so `ssh -O exit` can tear it
    //     down without touching the user's other SSH sessions.
    // ExitOnForwardFailure=yes: refuse to background if a -L fails to
    //     bind — turns races with `check_host_port_collisions` into
    //     loud errors rather than silently lost forwards.
    // The `ServerAlive*` bound that keeps this tunnel from outliving a dead
    // VM comes from `SshTarget::transport_opts` via `ssh_opts` above.
    args.extend([
        "-f".into(),
        "-N".into(),
        "-T".into(),
        "-o".into(),
        "ControlMaster=yes".into(),
        "-o".into(),
        format!("ControlPath={}", control_path.display()),
        "-o".into(),
        "ControlPersist=yes".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
    ]);

    let mut spec_log: Vec<String> = Vec::new();
    for f in forwards {
        let spec = format!("127.0.0.1:{}:127.0.0.1:{}", f.host.get(), f.guest.get());
        args.push("-L".into());
        args.push(spec.clone());
        spec_log.push(spec);
    }
    args.push(target.addr());

    tracing::info!("Establishing SSH port forwards: {}", spec_log.join(", "));

    let output = Command::new("ssh")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to launch ssh -L for port forwards")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ssh -L for port forwards exited with status: {}.\n\
             Forwards attempted: {}\n\
             stderr: {}",
            output.status,
            spec_log.join(", "),
            stderr.trim(),
        );
    }

    Ok(())
}

/// Best-effort cleanup: tell the parked control master to exit via
/// `ssh -O exit`, then remove the socket. Safe to call when no
/// forwards were ever started.
pub fn teardown_ssh_forwards(inst: &Instance, target: &SshTarget) {
    let control_path = forwards_control_path(inst);
    if !control_path.exists() {
        return;
    }

    tracing::debug!(
        "Tearing down SSH port forwards via control socket {}",
        control_path.display()
    );

    let control_arg = format!("ControlPath={}", control_path.display());
    let result = Command::new("ssh")
        .args(["-O", "exit", "-o", &control_arg, &target.addr()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match result {
        Ok(s) if s.success() => {
            tracing::debug!("SSH forwarder closed cleanly");
        }
        Ok(s) => {
            tracing::debug!("ssh -O exit returned {s} (non-fatal)");
        }
        Err(e) => {
            tracing::debug!("ssh -O exit failed: {e} (non-fatal)");
        }
    }

    if control_path.exists()
        && let Err(e) = fs::remove_file(&control_path)
    {
        tracing::debug!(
            "Failed to remove forward control socket {} (non-fatal): {e}",
            control_path.display()
        );
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
#[expect(clippy::expect_used, reason = "tests")]
#[expect(clippy::panic, reason = "tests use panic! for unreachable arms")]
mod tests {
    use super::*;

    fn pf(guest: u16, host: u16) -> PortForward {
        PortForward {
            guest: NonZeroU16::new(guest).expect("test inputs are non-zero"),
            host: NonZeroU16::new(host).expect("test inputs are non-zero"),
            label: None,
        }
    }

    #[test]
    fn collision_check_passes_when_ports_free() {
        let l1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let l2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let p1 = l1.local_addr().unwrap().port();
        let p2 = l2.local_addr().unwrap().port();
        drop(l1);
        drop(l2);
        let forwards = vec![pf(3000, p1), pf(4000, p2)];
        assert!(check_host_port_collisions(&forwards).is_ok());
    }

    #[test]
    fn collision_check_flags_in_use_host_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let forwards = vec![pf(3000, port)];
        let err = match check_host_port_collisions(&forwards) {
            Ok(()) => panic!("expected collision error"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains(&port.to_string()), "msg = {msg}");
        assert!(msg.contains("--forward-port"), "msg = {msg}");
    }

    #[test]
    fn collision_check_flags_internal_duplicate_host() {
        let forwards = vec![pf(3000, 9000), pf(3001, 9000)];
        let err = match check_host_port_collisions(&forwards) {
            Ok(()) => panic!("expected duplicate error"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("9000"), "msg = {msg}");
        assert!(msg.contains("Duplicate host port"), "msg = {msg}");
    }
}
