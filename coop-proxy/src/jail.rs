//! Self-confinement for `coop-proxy` (issue #411, slice 3).
//!
//! `coop-proxy` holds the real upstream credential and terminates connections
//! originated by the untrusted guest, so it is the feature's new attack
//! surface. This module bounds the blast radius of a proxy exploit: once
//! confined, the process cannot write any file, execute any program, or open a
//! TCP connection to anything but the two upstream ports (`443`) and DNS
//! (`53`). Reads stay open (the resolver config, the dynamic linker) and writes
//! to already-open descriptors (the inherited stderr log) are unaffected.
//!
//! **Linux** uses Landlock (ABI v4), applied by the process to *itself* from
//! [`crate::main`] — after its libraries are loaded and before the tokio
//! runtime spawns worker threads, so every thread inherits the domain. This
//! placement is deliberate: a launcher-side `pre_exec` grant of the filesystem
//! *execute* right cannot cover a dynamically linked binary (the kernel also
//! checks the right on the `ld.so` interpreter at `execve`), whereas a
//! post-startup self-restriction needs no execute grant at all — the proxy
//! never execs again.
//!
//! **macOS** is confined externally with a Seatbelt profile via `sandbox-exec`
//! (see coop's `proxy.rs`); there is nothing to do in-process there.
//!
//! **Limitations, stated honestly** (see `docs/trust-model.md`): the network
//! rules are port-scoped, not host-scoped — a fully compromised proxy could
//! still reach *some other* host on `:443` (the two upstreams' identity is
//! enforced at the TLS layer in [`crate::proxy`], and the guest still cannot
//! retarget them). Landlock's network rules cover TCP only, so UDP egress
//! (including DNS) is not restricted. Both match the DNS/CDN egress caveats
//! coop already documents.

use anyhow::Result;

/// A port outside the jail's allowlist, used by the self-test to prove a
/// connection to a non-upstream port is refused by the jail.
const PROBE_BLOCKED_PORT: u16 = 47821;

/// Run the production confinement ruleset against this process, then probe that
/// it actually restricts on this host: filesystem write denied, program exec
/// denied, a non-upstream TCP port refused, and the upstream port (`443`) still
/// reachable. Invoked via `coop-proxy --jail-selftest`; the VM integration
/// suite runs it on both backends (directly on Linux, under `sandbox-exec` on
/// macOS) so the jail is asserted, not merely designed.
///
/// Returns an error (non-zero exit) if any property does not hold, so the
/// integration test can assert on the exit status.
pub fn selftest() -> Result<()> {
    #[cfg(target_os = "linux")]
    apply(8788)?;
    probe()
}

/// Confine the current process. On Linux this establishes the Landlock domain
/// and **fails closed** if the kernel cannot fully enforce it. On other
/// platforms confinement is external and this is never called (the launcher
/// only requests it on Linux).
#[cfg(target_os = "linux")]
pub fn apply(listen_port: u16) -> Result<()> {
    linux::apply(listen_port)
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result, bail};
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };

    /// Upstream HTTPS. The guest never influences the host or scheme (see
    /// [`crate::proxy`]); this only widens the jail enough to reach it.
    const UPSTREAM_PORT: u16 = 443;
    /// DNS over TCP (the fallback path for truncated UDP responses). UDP DNS is
    /// not gated by Landlock's TCP-only network rules and needs no allowance.
    const DNS_TCP_PORT: u16 = 53;

    pub fn apply(listen_port: u16) -> Result<()> {
        let abi = ABI::V4;
        // Handle every filesystem-write and the execute right, plus TCP
        // bind/connect. Deliberately do not handle the read rights, so all
        // reads stay open (dynamic linker, `/etc/resolv.conf`). Nothing is
        // granted for write or execute → they are denied everywhere.
        let write_access = AccessFs::from_all(abi) & !AccessFs::from_read(abi);
        let status = Ruleset::default()
            // Fail closed on a kernel that cannot honour ABI v4 rather than
            // silently enforcing a weaker subset.
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(write_access)
            .context("handle filesystem-write access")?
            // `Execute` is a *read-family* right in Landlock (`from_read`
            // returns `Execute | ReadFile | ReadDir`), so `& !from_read` above
            // excluded it — it MUST be handled explicitly here or execve would
            // stay ungated and the jail would not block program execution. Do
            // not fold this into `write_access` or drop it as redundant.
            .handle_access(AccessFs::Execute)
            .context("handle filesystem-execute access")?
            .handle_access(AccessNet::ConnectTcp | AccessNet::BindTcp)
            .context("handle network access")?
            .create()
            .context("create Landlock ruleset")?
            // Egress allowlist: connect to the two upstream ports only.
            .add_rule(NetPort::new(UPSTREAM_PORT, AccessNet::ConnectTcp))
            .context("allow upstream connect")?
            .add_rule(NetPort::new(DNS_TCP_PORT, AccessNet::ConnectTcp))
            .context("allow DNS connect")?
            // Bind allowlist: only the proxy's own loopback listener.
            .add_rule(NetPort::new(listen_port, AccessNet::BindTcp))
            .context("allow listener bind")?
            .restrict_self()
            .context("apply Landlock restriction")?;

        if !matches!(status.ruleset, RulesetStatus::FullyEnforced) {
            bail!(
                "Landlock jail is {:?}, not fully enforced — the host kernel is too old \
                 (need ≥6.7 for network rules, ≥5.13 for filesystem rules). Refusing to run \
                 the credential proxy unconfined (fail-closed).",
                status.ruleset
            );
        }
        Ok(())
    }
}

/// Probe the four confinement properties on the current (already-confined)
/// process and report. Distinguishes a jail refusal (`PermissionDenied`) from a
/// mere connection refusal, so no live peers are needed.
fn probe() -> Result<()> {
    use std::io::ErrorKind::PermissionDenied;
    use std::net::TcpStream;

    let denied = |e: &std::io::Error| e.kind() == PermissionDenied;

    // (c) filesystem write denied
    let probe_file = std::env::temp_dir().join("coop-proxy-jail-selftest.probe");
    let write_blocked = match std::fs::File::create(&probe_file) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe_file);
            false
        }
        Err(e) => denied(&e),
    };

    // (d) program execution denied. `/bin/sh` exists on both supported hosts
    // (Linux, macOS); on a host without it execve would fail `NotFound` rather
    // than `PermissionDenied`, which reports FAIL (over-strict) — a false
    // negative, never a false pass.
    let exec_blocked = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .status()
        .is_err_and(|e| denied(&e));

    // (b) a non-upstream TCP port is refused by the jail
    let other_blocked =
        TcpStream::connect(("127.0.0.1", PROBE_BLOCKED_PORT)).is_err_and(|e| denied(&e));

    // (a) the upstream port (443) is still reachable — the jail must not block
    // it. A connection refusal (nothing listening on loopback:443) is fine;
    // only a jail refusal (PermissionDenied) would be a failure.
    let upstream_ok = !TcpStream::connect(("127.0.0.1", 443)).is_err_and(|e| denied(&e));

    let yn = |b: bool, yes: &'static str, no: &'static str| if b { yes } else { no };
    let pass = write_blocked && exec_blocked && other_blocked && upstream_ok;
    tracing::info!(
        "coop-proxy jail self-test: write={} exec={} connect-other={} connect-443={} => {}",
        yn(write_blocked, "BLOCKED", "OPEN"),
        yn(exec_blocked, "BLOCKED", "OPEN"),
        yn(other_blocked, "BLOCKED", "OPEN"),
        yn(upstream_ok, "ALLOWED", "BLOCKED"),
        yn(pass, "PASS", "FAIL"),
    );

    if pass {
        Ok(())
    } else {
        anyhow::bail!("coop-proxy jail self-test FAILED — the process is not properly confined")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must report failure when the process is *not* confined —
    /// otherwise a `=> PASS` from the integration self-test would be hollow.
    /// The test harness runs unconfined, so the write and exec probes succeed
    /// (nothing is blocked) and `probe` must return an error.
    #[test]
    fn probe_fails_when_unconfined() {
        assert!(
            probe().is_err(),
            "probe reported success in an unconfined process — the self-test would be hollow"
        );
    }
}
