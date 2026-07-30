//! Self-confinement for `coop-proxy` (issue #411, slice 3).
//!
//! `coop-proxy` holds the real upstream credential and terminates connections
//! originated by the untrusted guest, so it is the feature's new attack
//! surface. This module bounds the blast radius of a proxy exploit: once
//! confined, the process cannot write any file or execute any program, and —
//! on kernels new enough for Landlock's network rules — can open a TCP
//! connection only to the two upstream ports (`443`) and DNS (`53`). Reads stay
//! open (the resolver config, the dynamic linker) and writes to already-open
//! descriptors (the inherited stderr log) are unaffected.
//!
//! **Linux** uses Landlock, applied by the process to *itself* from
//! [`crate::main`] — after its libraries are loaded and before the tokio
//! runtime spawns worker threads, so every thread inherits the domain. The
//! confinement is **tiered by kernel capability** so it degrades gracefully
//! instead of failing closed on every host older than the newest ABI:
//!
//! - **Filesystem-write + program-exec — hard floor (kernel ≥5.13 / ABI v1).**
//!   Always enforced; if the kernel cannot honour it the proxy refuses to start
//!   (fail-closed). This is the high-value denial.
//! - **Extra filesystem-write rights added after v1 (refer v2, truncate v3) —
//!   best-effort.** Enforced where the kernel supports them, silently dropped
//!   on kernels 5.13–6.1.
//! - **TCP egress scoping — best-effort (kernel ≥6.7 / ABI v4).** Scopes
//!   outbound TCP to `:443`/`:53` where supported; on kernels 5.13–6.6 Landlock
//!   has no network rules, so this tier is dropped and TCP egress is left open.
//!   Upstream identity is still enforced at the TLS layer in [`crate::proxy`],
//!   and the guest still cannot retarget it.
//!
//! The placement (post-startup self-restriction) is deliberate: a launcher-side
//! `pre_exec` grant of the filesystem *execute* right cannot cover a
//! dynamically linked binary (the kernel also checks the right on the `ld.so`
//! interpreter at `execve`), whereas a post-startup self-restriction needs no
//! execute grant at all — the proxy never execs again.
//!
//! **macOS** is confined externally with a Seatbelt profile via `sandbox-exec`
//! (see coop's `proxy.rs`); there is nothing to do in-process there.
//!
//! **Limitations, stated honestly** (see `docs/trust-model.md`): the network
//! rules are port-scoped, not host-scoped — a fully compromised proxy could
//! still reach *some other* host on `:443` (the two upstreams' identity is
//! enforced at the TLS layer in [`crate::proxy`], and the guest still cannot
//! retarget them). Landlock's network rules cover TCP only, so UDP egress
//! (including DNS) is not restricted. And on kernels 5.13–6.6 the TCP tier is
//! absent entirely (open egress). All match the DNS/CDN egress caveats coop
//! already documents.

use anyhow::Result;

/// A port outside the jail's allowlist, used by the self-test to prove a
/// connection to a non-upstream port is refused by the jail.
const PROBE_BLOCKED_PORT: u16 = 47821;

/// Run the production confinement ruleset against this process, then probe that
/// it actually restricts on this host: filesystem write denied, program exec
/// denied, the upstream port (`443`) still reachable, and — when the network
/// tier is enforced — a non-upstream TCP port refused. Invoked via `coop-proxy
/// --jail-selftest`; the VM integration suite runs it on both backends
/// (directly on Linux, under `sandbox-exec` on macOS) so the jail is asserted,
/// not merely designed.
///
/// Returns an error (non-zero exit) if any property does not hold, so the
/// integration test can assert on the exit status.
pub fn selftest() -> Result<()> {
    // On Linux the applied jail reports whether its network tier survived on
    // this kernel. On macOS confinement is external (Seatbelt) and always
    // scopes the network, so the probe expects the non-upstream port blocked.
    #[cfg(target_os = "linux")]
    let net_scoped = apply(8788)?;
    #[cfg(not(target_os = "linux"))]
    let net_scoped = true;
    probe(net_scoped)
}

/// Confine the current process. On Linux this establishes the tiered Landlock
/// domain and **fails closed** only if the filesystem-write/program-exec floor
/// (ABI v1, kernel ≥5.13) cannot be enforced. Returns whether the best-effort
/// TCP tier was scoped on this kernel (`true` = egress limited to `:443`/`:53`;
/// `false` = kernel 5.13–6.6, network tier dropped, egress open). On other
/// platforms confinement is external and this is never called (the launcher
/// only requests it on Linux).
#[cfg(target_os = "linux")]
pub fn apply(listen_port: u16) -> Result<bool> {
    linux::apply(listen_port)
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result, bail};
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, LandlockStatus, NetPort,
        Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    };

    /// Upstream HTTPS. The guest never influences the host or scheme (see
    /// [`crate::proxy`]); this only widens the jail enough to reach it.
    const UPSTREAM_PORT: u16 = 443;
    /// DNS over TCP (the fallback path for truncated UDP responses). UDP DNS is
    /// not gated by Landlock's TCP-only network rules and needs no allowance.
    const DNS_TCP_PORT: u16 = 53;

    /// Whether the TCP-egress tier scopes ports at the given effective ABI.
    /// Landlock's network rules require ABI v4 (kernel ≥6.7); below that the
    /// tier is silently dropped and egress is left open.
    pub(super) fn net_scoped_at(effective_abi: ABI) -> bool {
        effective_abi >= ABI::V4
    }

    /// Build three stacked Landlock domains, each single-purpose so its status
    /// is checkable in isolation (domains accumulate across `restrict_self`
    /// calls):
    ///
    /// 1. filesystem-write + program-exec at ABI v1, `HardRequirement` — the
    ///    fail-closed floor;
    /// 2. the extra filesystem-write rights added after v1 (refer v2, truncate
    ///    v3), `BestEffort`;
    /// 3. TCP bind/connect + the upstream port allowlist, `BestEffort`.
    ///
    /// Returns whether the network tier was actually enforced on this kernel.
    pub fn apply(listen_port: u16) -> Result<bool> {
        let v1 = ABI::V1;
        // The v1 write/make rights: every v1 access minus the read family.
        // Deliberately do not handle the read rights, so all reads stay open
        // (dynamic linker, `/etc/resolv.conf`). Nothing is granted for these →
        // they are denied everywhere.
        let fs_write_v1 = AccessFs::from_all(v1) & !AccessFs::from_read(v1);
        let floor = Ruleset::default()
            // Fail closed on a kernel too old for the v1 filesystem/exec floor
            // rather than silently enforcing nothing.
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(fs_write_v1)
            .context("handle filesystem-write access (v1 floor)")?
            // `Execute` is a *read-family* right in Landlock (`from_read`
            // returns `Execute | ReadFile | ReadDir`), so `& !from_read` above
            // excluded it — it MUST be handled explicitly here or execve would
            // stay ungated and the jail would not block program execution. Do
            // not fold this into `fs_write_v1` or drop it as redundant.
            .handle_access(AccessFs::Execute)
            .context("handle filesystem-execute access (v1 floor)")?
            .create()
            .context("create Landlock filesystem/exec floor ruleset")?
            .restrict_self()
            .context("apply Landlock filesystem/exec floor")?;
        // This ruleset's hard content is only the v1 fs+exec floor, so anything
        // short of full enforcement means a <5.13 kernel (or the hard handles
        // errored): fail closed.
        if !matches!(floor.ruleset, RulesetStatus::FullyEnforced) {
            bail!(
                "Landlock filesystem/exec floor is {:?}, not fully enforced — the host kernel \
                 is older than 5.13 (Landlock ABI v1). Refusing to run the credential proxy \
                 without the filesystem-write and program-exec jail (fail-closed).",
                floor.ruleset
            );
        }

        // The filesystem-write rights introduced after v1 (refer v2, truncate
        // v3): all fs rights up to v4, minus the v1 set already handled above,
        // minus the read family. Best-effort so a 5.13–6.1 kernel drops them
        // instead of failing.
        let v4 = ABI::V4;
        let fs_write_above_v1 =
            AccessFs::from_all(v4) & !AccessFs::from_all(v1) & !AccessFs::from_read(v4);
        Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(fs_write_above_v1)
            .context("handle post-v1 filesystem-write access")?
            .create()
            .context("create post-v1 filesystem-write ruleset")?
            .restrict_self()
            .context("apply post-v1 filesystem-write restriction")?;

        // TCP egress scoping. Best-effort so kernels 5.13–6.6 (no Landlock
        // network rules) drop it and leave TCP open rather than failing. The
        // `handle_access(AccessNet…)` call is required even under best-effort or
        // the `NetPort` rules below fail the consistency check.
        let net = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessNet::ConnectTcp | AccessNet::BindTcp)
            .context("handle network access")?
            .create()
            .context("create Landlock network ruleset")?
            // Egress allowlist: connect to the two upstream ports only.
            .add_rule(NetPort::new(UPSTREAM_PORT, AccessNet::ConnectTcp))
            .context("allow upstream connect")?
            .add_rule(NetPort::new(DNS_TCP_PORT, AccessNet::ConnectTcp))
            .context("allow DNS connect")?
            // Bind allowlist: only the proxy's own loopback listener.
            .add_rule(NetPort::new(listen_port, AccessNet::BindTcp))
            .context("allow listener bind")?
            .restrict_self()
            .context("apply Landlock network restriction")?;

        // Whether the net tier actually took effect is decided by the ABI the
        // running kernel supported. A non-`Available` status cannot occur here
        // (the v1 hard floor above already proved Landlock is usable), but map
        // it to the degraded/open case rather than assuming.
        let effective_abi = match net.landlock {
            LandlockStatus::Available { effective_abi, .. } => effective_abi,
            LandlockStatus::NotEnabled | LandlockStatus::NotImplemented => ABI::Unsupported,
        };
        Ok(net_scoped_at(effective_abi))
    }
}

/// Probe the confinement properties on the current (already-confined) process
/// and report. Distinguishes a jail refusal (`PermissionDenied`) from a mere
/// connection refusal, so no live peers are needed.
///
/// `net_scoped` says whether the TCP-egress tier is active on this host: the
/// non-upstream-port block is only asserted when it is `true` (on a degraded
/// host that port is legitimately open).
fn probe(net_scoped: bool) -> Result<()> {
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

    // (b) a non-upstream TCP port is refused by the jail. Only meaningful when
    // the network tier is enforced; on a degraded kernel the port is open and
    // that is the expected, acceptable outcome.
    let other_blocked =
        TcpStream::connect(("127.0.0.1", PROBE_BLOCKED_PORT)).is_err_and(|e| denied(&e));
    let net_ok = !net_scoped || other_blocked;

    // (a) the upstream port (443) is still reachable — the jail must not block
    // it. A connection refusal (nothing listening on loopback:443) is fine;
    // only a jail refusal (PermissionDenied) would be a failure.
    let upstream_ok = !TcpStream::connect(("127.0.0.1", 443)).is_err_and(|e| denied(&e));

    let yn = |b: bool, yes: &'static str, no: &'static str| if b { yes } else { no };
    let pass = write_blocked && exec_blocked && net_ok && upstream_ok;
    tracing::info!(
        "coop-proxy jail self-test: write={} exec={} connect-other={} connect-443={} \
         net-scoping={} => {}",
        yn(write_blocked, "BLOCKED", "OPEN"),
        yn(exec_blocked, "BLOCKED", "OPEN"),
        yn(other_blocked, "BLOCKED", "OPEN"),
        yn(upstream_ok, "ALLOWED", "BLOCKED"),
        yn(net_scoped, "enforced", "degraded"),
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
    /// (nothing is blocked) and `probe` must return an error. It is passed
    /// `false` (net tier degraded) so the network check is lenient and the
    /// failure it asserts comes from the filesystem/exec probes, not the port
    /// probe.
    #[test]
    fn probe_fails_when_unconfined() {
        assert!(
            probe(false).is_err(),
            "probe reported success in an unconfined process — the self-test would be hollow"
        );
    }

    /// The net-scoping threshold is exactly ABI v4 (kernel ≥6.7): v4 and newer
    /// scope egress, everything below (including an unsupported kernel) degrades
    /// to open egress.
    #[cfg(target_os = "linux")]
    #[test]
    fn net_scoped_threshold_is_abi_v4() {
        use landlock::ABI;
        assert!(linux::net_scoped_at(ABI::V4));
        assert!(linux::net_scoped_at(ABI::V5));
        assert!(!linux::net_scoped_at(ABI::V3));
        assert!(!linux::net_scoped_at(ABI::V1));
        assert!(!linux::net_scoped_at(ABI::Unsupported));
    }
}
