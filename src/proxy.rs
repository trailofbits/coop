//! Host-side lifecycle for the credential-injecting proxy (issue #411).
//!
//! `coop-proxy` is a separate binary (its own workspace crate) that runs on
//! the host for the lifetime of a remote-mode VM. This module resolves the
//! real credential, mints a per-instance capability token, spawns the proxy
//! process (bound on host loopback), and exposes it into the guest with a
//! per-instance `ssh -R` reverse tunnel — both tracked by PID files.
//!
//! One `coop-proxy` process (and one reverse tunnel) runs per (VM, provider):
//! Anthropic for Claude Code, `OpenAI` for Codex. The host lifecycle is shared,
//! but `coop-proxy` applies a provider-specific operation profile as well as a
//! fixed upstream and authentication scheme. Adding a provider therefore
//! requires an explicit proxy policy change, not just a host, port, and auth
//! configuration. See [`docs/design/issue-411-injecting-proxy.md`].
//!
//! Binding host loopback + reverse-tunnelling works identically on both
//! backends (Firecracker and Lima), keeps the listener off every non-loopback
//! interface, and gives each guest its own tunnel (no shared-bridge exposure).
//! The guest is pointed at `http://127.0.0.1:<port>` and holds only the
//! capability token, which the proxy verifies before injecting the real
//! credential upstream.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::backend::SshTarget;
use crate::config::{Instance, ProxyAuthScheme, ProxyUpstream, Secret, resolve_cmd_value};

/// The proxy binary name, expected next to the `coop` binary.
const PROXY_BIN_NAME: &str = "coop-proxy";

/// Maximum time for SSH authentication and the guest's forwarding acknowledgment.
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the freshly spawned proxy to accept a connection on
/// its host-loopback listener before treating the launch as failed. This
/// enforces fail-closed startup: when confinement *fails to establish* the
/// proxy exits before binding (an unsupported kernel makes Landlock's
/// `apply` bail on Linux; a missing `sandbox-exec` or a malformed Seatbelt
/// profile makes the wrapper exit non-zero on macOS), so this probe never
/// connects and the VM start aborts rather than proceeding with a dead
/// credential proxy. It confirms the proxy *bound* — the *strength* of the
/// confinement is asserted separately by `coop-proxy --jail-selftest` in the
/// integration suite, not by liveness here.
const PROXY_READY_GRACE: Duration = Duration::from_secs(5);

/// A proxied upstream. This enum carries the host-side per-provider constants
/// (upstream host, base port, file/tunnel name); `coop-proxy` separately owns
/// the per-provider operation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Anthropic (Claude Code) → `api.anthropic.com`.
    Anthropic,
    /// `OpenAI` (Codex) → `api.openai.com`.
    Openai,
}

impl Provider {
    /// Every provider, for teardown that must reach all of them.
    pub const ALL: [Provider; 2] = [Provider::Anthropic, Provider::Openai];

    /// Short name used in PID/log/token file names and log lines.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::Openai => "openai",
        }
    }

    /// The fixed upstream host. The guest cannot influence this — only the
    /// request path is forwarded (closes SSRF).
    fn upstream_host(self) -> &'static str {
        match self {
            Provider::Anthropic => "api.anthropic.com",
            Provider::Openai => "api.openai.com",
        }
    }

    /// Base port for this provider's per-instance proxy. The actual port is
    /// `base + instance index`, so concurrent VMs never collide on host
    /// loopback. The instance index spans `0..=252`, so the bases are 1000
    /// apart to keep the two providers' ranges disjoint (Anthropic
    /// `8788..=9040`, `OpenAI` `9788..=10040`).
    fn base_port(self) -> u16 {
        match self {
            Provider::Anthropic => 8788,
            Provider::Openai => 9788,
        }
    }

    /// Per-instance listen port: base + index.
    fn port(self, inst: &Instance) -> u16 {
        self.base_port().saturating_add(inst.index.as_u16())
    }

    /// Whether the capability token must be persisted host-side for later
    /// sessions to read. Codex needs it: the token is sent as the provider's
    /// bearer `env_key`, forwarded via `SendEnv` on every session (which is
    /// created before bootstrap mints the token). Claude Code does not — its
    /// token rides in the guest's `settings.json`, written at bootstrap — so
    /// its token stays in host memory only (unchanged from slice 1).
    fn persists_token(self) -> bool {
        matches!(self, Provider::Openai)
    }
}

/// A running proxy and the values the guest config needs.
#[derive(Debug, Clone)]
pub struct ProxyHandle {
    /// Base URL the guest is pointed at, e.g. `http://127.0.0.1:8788`.
    pub base_url: String,
    /// Per-instance capability token the guest presents (as a bearer / API
    /// key), which the proxy verifies before injecting the real credential.
    /// Wrapped so a stray `Debug` of the handle never leaks it, matching the
    /// hygiene of the credential-adjacent path.
    pub capability_token: Secret<String>,
}

/// Start the proxy for one `provider` on `inst`: resolve the credential, bind
/// the proxy on host loopback, and open a reverse SSH tunnel via `target` so
/// the guest reaches it at `127.0.0.1:<port>`.
///
/// **Fail closed:** if the credential cannot be resolved (or the tunnel cannot
/// be established), this returns an error and the VM start is aborted — the
/// guest never comes up on a path where the agent silently has no or the wrong
/// credential.
pub fn start_provider(
    inst: &Instance,
    provider: Provider,
    upstream: &ProxyUpstream,
    target: &SshTarget,
) -> Result<ProxyHandle> {
    let credential = resolve_cmd_value(upstream.credential.expose()).with_context(|| {
        format!(
            "Failed to resolve the {} proxy credential — aborting VM start (fail-closed); \
             the guest must never come up without the injected credential",
            provider.name()
        )
    })?;

    let token = mint_capability_token()?;
    let port = provider.port(inst);
    let listen = SocketAddr::from(([127, 0, 0, 1], port));
    let json = wire_config_json(
        &listen,
        &token,
        provider.upstream_host(),
        upstream.auth,
        &credential,
    )?;

    spawn_proxy(inst, provider.name(), listen, &json)?;
    // Persist the capability token for providers that forward it via env on
    // later sessions (Codex); Claude reads its token from the returned handle
    // (settings.json) and keeps it out of host disk. Written after spawn so a
    // stale token is never left pointing at a dead proxy.
    if provider.persists_token()
        && let Err(e) = write_token_file(inst, provider, &token)
    {
        stop_provider(inst, provider);
        return Err(e);
    }
    // Expose the loopback proxy into the guest. If the tunnel fails, tear the
    // proxy back down so we don't leave it orphaned (fail closed).
    if let Err(e) = spawn_reverse_forward(inst, provider.name(), target, port) {
        stop_provider(inst, provider);
        return Err(e);
    }
    tracing::info!(
        "Started {} credential proxy on {listen} (guest → 127.0.0.1:{port})",
        provider.name()
    );

    Ok(ProxyHandle {
        base_url: format!("http://127.0.0.1:{port}"),
        capability_token: Secret::new(token),
    })
}

/// Tear down the proxy process, tunnel, and token file for one `provider`.
/// Best-effort, safe to call when none were started.
pub fn stop_provider(inst: &Instance, provider: Provider) {
    let name = provider.name();
    kill_pid_file(&pid_path(inst, name), "proxy");
    kill_pid_file(&fwd_pid_path(inst, name), "proxy tunnel");
    let token = token_path(inst, name);
    if token.exists()
        && let Err(e) = fs::remove_file(&token)
    {
        tracing::debug!(
            "Failed to remove proxy token file {} (non-fatal): {e}",
            token.display()
        );
    }
}

/// Tear down every provider's proxy for `inst` (stop/destroy). Best-effort.
pub fn stop(inst: &Instance) {
    for provider in Provider::ALL {
        stop_provider(inst, provider);
    }
}

/// The persisted capability token for a running `provider` proxy, if any.
/// Used to forward the Codex provider bearer on interactive sessions after
/// bootstrap has started the proxy.
pub fn read_capability_token(inst: &Instance, provider: Provider) -> Option<String> {
    let token = fs::read_to_string(token_path(inst, provider.name())).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

// ── process supervision ──────────────────────────────────────

fn spawn_proxy(inst: &Instance, name: &str, listen: SocketAddr, json: &str) -> Result<()> {
    // A previous run may have crashed without stop cleanup; kill any leftover
    // before binding so we don't race it for the same port.
    kill_pid_file(&pid_path(inst, name), "stale proxy");

    let bin = locate_proxy_binary()?;
    let log_path = inst.dir.join(format!("proxy-{name}.log"));
    let log = File::create(&log_path)
        .with_context(|| format!("Failed to create proxy log {}", log_path.display()))?;

    let mut child = confined_command(&bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        // New process group so a Ctrl-C to the coop CLI's group doesn't kill
        // the proxy; it must outlive the foreground command.
        .process_group(0)
        .spawn()
        .with_context(|| format!("Failed to spawn {}", bin.display()))?;

    let mut stdin = child
        .stdin
        .take()
        .context("proxy child stdin unexpectedly missing")?;
    stdin
        .write_all(json.as_bytes())
        .context("Failed to write proxy startup config to stdin")?;
    // Drop closes stdin (EOF) so the proxy finishes reading its config.
    drop(stdin);

    // Fail closed: confirm the proxy bound its listener before recording it.
    // When confinement (Landlock on Linux, Seatbelt on macOS) cannot be
    // established the proxy exits before binding, so this probe never connects
    // and the launch aborts — a credential proxy that could not be jailed never
    // reaches a serving state. Kill the child on failure so it is not orphaned.
    if let Err(e) = await_proxy_ready(&mut child, listen, &log_path) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }

    let pid = child.id();
    let pid_path = pid_path(inst, name);
    if let Err(e) = fs::write(&pid_path, pid.to_string()) {
        // Without a pid file `stop_provider` can never reap this proxy, and the
        // std `Child` destructor does not kill it — so a running proxy holding
        // the real credential would be orphaned. Kill it before failing.
        let _ = child.kill();
        let _ = child.wait();
        return Err(e)
            .with_context(|| format!("Failed to write proxy pid file {}", pid_path.display()));
    }
    // Deliberately do not wait past this point: the proxy runs for the VM's
    // lifetime, and the std `Child` destructor does not kill it.
    Ok(())
}

/// Poll until the proxy accepts a connection on its host-loopback listener, or
/// fail closed. Returns an error (carrying the proxy log) if the proxy exits
/// early or never begins serving within [`PROXY_READY_GRACE`].
fn await_proxy_ready(
    child: &mut std::process::Child,
    listen: SocketAddr,
    log_path: &Path,
) -> Result<()> {
    let deadline = Instant::now() + PROXY_READY_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let log = fs::read_to_string(log_path).unwrap_or_default();
                bail!(
                    "credential proxy exited before it began serving ({status}) — its \
                     confinement or bind failed; refusing to start the VM (fail-closed).\n{}",
                    log.trim()
                );
            }
            Ok(None) => {}
            Err(e) => return Err(e).context("Failed to poll the credential proxy process"),
        }
        if std::net::TcpStream::connect_timeout(&listen, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let log = fs::read_to_string(log_path).unwrap_or_default();
            bail!(
                "credential proxy did not begin serving on {listen} within \
                 {PROXY_READY_GRACE:?} — refusing to start the VM (fail-closed).\n{}",
                log.trim()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Build the `Command` that launches `coop-proxy` under the platform's process
/// confinement (issue #411, slice 3).
///
/// On Linux the proxy self-confines with Landlock unconditionally (it never
/// receives a `--no-jail` opt-out from `coop`), so the command is the binary
/// itself. On macOS — which has no first-class in-process sandbox — the
/// launcher wraps it in `sandbox-exec` with a Seatbelt profile
/// ([`SEATBELT_PROFILE`]) that denies filesystem writes and program execution
/// and limits egress to the two upstream ports. `sandbox-exec`
/// `execve`-replaces itself with the proxy, so the spawned pid is the proxy's
/// and teardown is unchanged.
fn confined_command(bin: &Path) -> Command {
    #[cfg(target_os = "macos")]
    {
        // sandbox-exec applies the profile to itself, then execve-replaces
        // itself with the proxy — an exec the profile must permit. The profile
        // scopes that allowance to this exact binary via the PROXY_BIN
        // parameter, so nothing else can be exec'd. Canonicalize so the path
        // matches what the kernel resolves at exec time (e.g. /var →
        // /private/var); fall back to the given path if canonicalization fails.
        let resolved = fs::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
        let mut cmd = Command::new("sandbox-exec");
        cmd.arg("-D")
            .arg(format!("PROXY_BIN={}", resolved.display()))
            .arg("-p")
            .arg(SEATBELT_PROFILE)
            .arg(&resolved);
        cmd
    }
    #[cfg(not(target_os = "macos"))]
    {
        Command::new(bin)
    }
}

/// Seatbelt profile confining `coop-proxy` on macOS. Kept as a checked-in
/// `.sb` file so it is the single source of truth shared by the launcher
/// (here) and the integration smoke test (`sandbox-exec -f src/seatbelt-proxy.sb
/// coop-proxy --jail-selftest`). Denies filesystem writes, program exec, and
/// all egress except `:443`/`:53`; see the file for the full rationale and the
/// `sandbox-exec`-deprecation trade-off.
#[cfg(target_os = "macos")]
const SEATBELT_PROFILE: &str = include_str!("seatbelt-proxy.sb");

/// Establish a per-instance reverse SSH tunnel so the guest reaches the
/// host-loopback proxy at `127.0.0.1:{port}` (`ssh -R guest → host`). Detached
/// and tracked by a PID file like the proxy process, so teardown needs no SSH
/// target. An acknowledged forwarding request makes a bind clash on the guest
/// a startup failure rather than a silently dead tunnel.
fn spawn_reverse_forward(inst: &Instance, name: &str, target: &SshTarget, port: u16) -> Result<()> {
    kill_pid_file(&fwd_pid_path(inst, name), "stale proxy tunnel");

    let control_dir = create_tunnel_control_dir()?;
    let control_path = control_dir.path().join("ssh.sock");
    let log_path = inst.dir.join(format!("proxy-{name}-fwd.log"));
    let log = File::create(&log_path)
        .with_context(|| format!("Failed to create proxy tunnel log {}", log_path.display()))?;
    let forward_log = log
        .try_clone()
        .context("Failed to clone proxy tunnel log")?;

    let mut args = target.ssh_opts();
    args.extend([
        "-N".into(),
        "-T".into(),
        "-o".into(),
        "ControlMaster=yes".into(),
        "-o".into(),
        "ControlPersist=no".into(),
        "-o".into(),
        "ForkAfterAuthentication=no".into(),
        "-S".into(),
        control_path.display().to_string(),
    ]);
    args.push(target.addr());

    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        // New process group so the tunnel outlives the foreground command.
        .process_group(0)
        .spawn()
        .context("Failed to spawn the reverse SSH tunnel for the credential proxy")?;

    let setup = (|| -> Result<()> {
        let deadline = Instant::now() + TUNNEL_READY_TIMEOUT;
        loop {
            crate::signal::check_shutdown()?;
            if let Some(status) = child.try_wait().context("Failed to poll proxy tunnel")? {
                bail!(
                    "reverse SSH tunnel exited before its control socket became ready ({status})"
                );
            }
            if control_path.exists() {
                break;
            }
            if Instant::now() >= deadline {
                bail!("Timed out waiting for proxy tunnel authentication");
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // A live SSH process or control socket does not prove the guest has
        // accepted a reverse forward. -O forward waits for that acknowledgment
        // and returns nonzero for a rejected bind. The master stays the direct
        // child, so its existing PID-file teardown remains authoritative.
        let mut forward_args = target.ssh_opts();
        forward_args.extend([
            "-S".into(),
            control_path.display().to_string(),
            "-O".into(),
            "forward".into(),
            "-R".into(),
            format!("127.0.0.1:{port}:127.0.0.1:{port}"),
        ]);
        forward_args.push(target.addr());
        let request = Command::new("ssh")
            .args(&forward_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(forward_log))
            .spawn()
            .context("Failed to request the credential proxy's reverse forward")?;
        await_forwarding_ack(request, deadline)?;

        let path = fwd_pid_path(inst, name);
        fs::write(&path, child.id().to_string())
            .with_context(|| format!("Failed to write proxy tunnel pid file {}", path.display()))?;
        Ok(())
    })();
    if let Err(e) = setup {
        let _ = child.kill();
        let _ = child.wait();
        let log = fs::read_to_string(&log_path).unwrap_or_default();
        return Err(e).context(format!(
            "Failed to establish the credential proxy's reverse SSH tunnel.\n{}",
            log.trim()
        ));
    }
    // The master no longer needs a control socket: later teardown uses its PID.
    Ok(())
}

/// Use a short, private path even when TMPDIR or the instance path is long.
fn create_tunnel_control_dir() -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("coop-proxy-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .context("Failed to create proxy tunnel control directory")
}

/// Reap the setup client on every path, including a guest that never replies.
fn await_forwarding_ack(mut request: std::process::Child, deadline: Instant) -> Result<()> {
    let result = (|| -> Result<()> {
        loop {
            crate::signal::check_shutdown()?;
            if let Some(status) = request
                .try_wait()
                .context("Failed to poll forwarding request")?
            {
                anyhow::ensure!(
                    status.success(),
                    "Proxy reverse forwarding request failed ({status})"
                );
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("Timed out waiting for proxy reverse forwarding acknowledgment");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    })();
    if result.is_err() {
        let _ = request.kill();
        let _ = request.wait();
    }
    result
}

/// SIGTERM the process named by a PID file and remove the file. Best-effort;
/// safe when the file is absent or malformed.
fn kill_pid_file(path: &Path, label: &str) {
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    match contents.trim().parse::<i32>() {
        Ok(pid) if pid > 0 => {
            // Safety: `kill` with a parsed positive pid and a constant signal.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            tracing::debug!("Sent SIGTERM to {label} (pid {pid})");
        }
        _ => tracing::debug!("Ignoring malformed pid file {}", path.display()),
    }
    if let Err(e) = fs::remove_file(path) {
        tracing::debug!(
            "Failed to remove pid file {} (non-fatal): {e}",
            path.display()
        );
    }
}

/// Persist the capability token owner-only (0600). It is worthless off the
/// host, but kept owner-only for consistency with other per-instance state.
fn write_token_file(inst: &Instance, provider: Provider, token: &str) -> Result<()> {
    let path = token_path(inst, provider.name());
    crate::fs_util::atomic_write_with_mode(&path, token, 0o600)
        .with_context(|| format!("Failed to write proxy token file {}", path.display()))
}

fn pid_path(inst: &Instance, name: &str) -> PathBuf {
    inst.dir.join(format!("proxy-{name}.pid"))
}

fn fwd_pid_path(inst: &Instance, name: &str) -> PathBuf {
    inst.dir.join(format!("proxy-{name}-fwd.pid"))
}

fn token_path(inst: &Instance, name: &str) -> PathBuf {
    inst.dir.join(format!("proxy-{name}.token"))
}

fn locate_proxy_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Failed to locate the coop executable")?;
    let dir = exe
        .parent()
        .context("coop executable has no parent directory")?;
    let candidate = dir.join(PROXY_BIN_NAME);
    if candidate.exists() {
        return Ok(candidate);
    }
    bail!(
        "{PROXY_BIN_NAME} not found next to coop at {} — reinstall coop; \
         the proxy ships in the same tarball as coop",
        candidate.display()
    );
}

// ── pure helpers ─────────────────────────────────────────────

/// Mint a 256-bit capability token from the OS CSPRNG, hex-encoded. Worthless
/// off the host, so exfiltration by a compromised guest gains nothing.
fn mint_capability_token() -> Result<String> {
    let mut buf = [0u8; 32];
    let mut urandom = File::open("/dev/urandom").context("Failed to open /dev/urandom")?;
    urandom
        .read_exact(&mut buf)
        .context("Failed to read from /dev/urandom")?;
    Ok(hex::encode(buf))
}

/// The stdin startup blob for `coop-proxy`, matching its `ProxyConfig` shape.
/// Kept as a private mirror rather than a shared crate so the `coop` binary's
/// dependency closure stays free of the proxy's async/HTTP/TLS stack.
#[derive(Serialize)]
struct WireConfig<'a> {
    listen: String,
    capability_token: &'a str,
    upstream_host: &'a str,
    injection: WireInjection<'a>,
}

#[derive(Serialize)]
#[serde(tag = "scheme", rename_all = "snake_case")]
enum WireInjection<'a> {
    XApiKey { credential: &'a str },
    Bearer { credential: &'a str },
}

fn wire_config_json(
    listen: &SocketAddr,
    capability_token: &str,
    upstream_host: &str,
    auth: ProxyAuthScheme,
    credential: &str,
) -> Result<String> {
    let injection = match auth {
        ProxyAuthScheme::ApiKey => WireInjection::XApiKey { credential },
        ProxyAuthScheme::Bearer => WireInjection::Bearer { credential },
    };
    let wire = WireConfig {
        listen: listen.to_string(),
        capability_token,
        upstream_host,
        injection,
    };
    serde_json::to_string(&wire).context("Failed to serialize proxy startup config")
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::config::{ImageName, InstanceIndex, InstanceName};

    fn inst_with_index(index: u16) -> Instance {
        Instance {
            name: InstanceName::new("t").unwrap(),
            index: InstanceIndex::new(index).unwrap(),
            dir: PathBuf::from("/tmp/coop-test"),
            image: ImageName::new("t.img").unwrap(),
        }
    }

    #[test]
    fn ports_are_per_instance_and_per_provider() {
        assert_eq!(Provider::Anthropic.port(&inst_with_index(0)), 8788);
        assert_eq!(Provider::Anthropic.port(&inst_with_index(5)), 8793);
        assert_eq!(Provider::Openai.port(&inst_with_index(0)), 9788);
        assert_eq!(Provider::Openai.port(&inst_with_index(5)), 9793);
    }

    #[test]
    fn provider_ranges_do_not_overlap() {
        // 252 is the max instance index (0..=252); the Anthropic range top
        // must stay strictly below the OpenAI base so no (VM, provider) pair
        // ever shares a host-loopback port.
        assert!(Provider::Anthropic.base_port() + 252 < Provider::Openai.base_port());
    }

    #[test]
    fn upstream_hosts_are_pinned() {
        assert_eq!(Provider::Anthropic.upstream_host(), "api.anthropic.com");
        assert_eq!(Provider::Openai.upstream_host(), "api.openai.com");
    }

    #[test]
    fn only_openai_persists_its_token() {
        // Codex forwards the token via env on later sessions, so it must be
        // persisted; Claude reads its token from settings.json and keeps it in
        // host memory only (unchanged from slice 1).
        assert!(Provider::Openai.persists_token());
        assert!(!Provider::Anthropic.persists_token());
    }

    #[test]
    fn capability_token_is_64_hex_and_random() {
        let a = mint_capability_token().unwrap();
        let b = mint_capability_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "tokens must not repeat");
    }

    #[test]
    fn wire_json_api_key_shape() {
        let listen: SocketAddr = "172.16.0.1:8788".parse().unwrap();
        let json = wire_config_json(
            &listen,
            "cap-tok",
            "api.anthropic.com",
            ProxyAuthScheme::ApiKey,
            "sk-real",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["listen"], "172.16.0.1:8788");
        assert_eq!(v["capability_token"], "cap-tok");
        assert_eq!(v["upstream_host"], "api.anthropic.com");
        assert_eq!(v["injection"]["scheme"], "x_api_key");
        assert_eq!(v["injection"]["credential"], "sk-real");
    }

    #[test]
    fn wire_json_bearer_shape() {
        let listen: SocketAddr = "172.16.0.1:8900".parse().unwrap();
        let json = wire_config_json(
            &listen,
            "t",
            "api.openai.com",
            ProxyAuthScheme::Bearer,
            "sk-openai",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["injection"]["scheme"], "bearer");
        assert_eq!(v["injection"]["credential"], "sk-openai");
    }

    #[test]
    fn await_proxy_ready_fails_closed_when_proxy_exits_early() {
        // A proxy that exits before binding (e.g. the jail could not be
        // established) must abort the launch, not proceed. Stand in with a
        // child that exits immediately; `await_proxy_ready` must return Err.
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("proxy.log");
        std::fs::write(&log, "boom: jail could not be established\n").unwrap();
        // An address nothing will ever bind, so success can only come from the
        // (never-happening) listener, not a stray connect.
        let listen: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut child = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
        let err = await_proxy_ready(&mut child, listen, &log).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exited before it began serving"),
            "unexpected error: {msg}"
        );
        let _ = child.wait();
    }

    #[test]
    fn tunnel_control_directory_is_owner_only() {
        let dir = create_tunnel_control_dir().unwrap();
        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn forwarding_ack_waits_for_success_and_rejects_failure() {
        let start = Instant::now();
        let success = Command::new("sh")
            .args(["-c", "sleep 0.2; exit 0"])
            .spawn()
            .unwrap();
        await_forwarding_ack(success, start + Duration::from_secs(5)).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(200));

        let failure = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
        let error =
            await_forwarding_ack(failure, Instant::now() + Duration::from_secs(5)).unwrap_err();
        assert!(error.to_string().contains("request failed"));
    }

    #[test]
    fn forwarding_ack_timeout_reaps_setup_client() {
        let request = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = i32::try_from(request.id()).unwrap();
        let error =
            await_forwarding_ack(request, Instant::now() + Duration::from_millis(200)).unwrap_err();
        assert!(error.to_string().contains("Timed out"));
        // The setup client is gone, not merely dropped and left running.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn reverse_forward_rejects_a_live_ssh_without_an_established_connection() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut inst = inst_with_index(0);
        inst.dir = tmp.path().to_path_buf();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let peer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((connection, _)) => {
                        // Outlive the old two-second readiness grace, then fail
                        // the handshake without ever acknowledging a forward.
                        std::thread::sleep(Duration::from_millis(2300));
                        drop(connection);
                        return Ok(true);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return Ok(false);
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => return Err(e),
                }
            }
        });
        let target = SshTarget {
            host: crate::backend::Hostname::new("127.0.0.1").unwrap(),
            port: std::num::NonZeroU16::new(port).unwrap(),
            user: crate::backend::SshUser::new("coop").unwrap(),
            key_path: tmp.path().join("unused-key"),
        };
        let result = spawn_reverse_forward(&inst, "test", &target, 8788);
        let accepted = peer.join().unwrap().unwrap();
        // Also clean up the old implementation when deliberately regressed.
        kill_pid_file(&fwd_pid_path(&inst, "test"), "test tunnel");
        assert!(accepted, "SSH must have reached the stalled peer");
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("before its control socket became ready"));
        assert!(!fwd_pid_path(&inst, "test").exists());
    }

    // The runner supplies real OpenSSH in disposable network/PID namespaces:
    // the host destination and guest reverse listener use the same port.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires tests/integration-proxy-forward.sh"]
    fn reverse_forward_requires_authenticated_bind_acknowledgment() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let fixture = PathBuf::from(std::env::var("COOP_FORWARD_TEST_DIR").unwrap());
        let tmp = tempfile::TempDir::new().unwrap();
        let mut inst = inst_with_index(0);
        inst.dir = tmp.path().to_path_buf();
        let target = SshTarget {
            host: crate::backend::Hostname::new("192.0.2.2").unwrap(),
            port: std::num::NonZeroU16::new(2222).unwrap(),
            user: crate::backend::SshUser::new("root").unwrap(),
            key_path: fixture.join("client"),
        };
        let master_pid = || -> i32 {
            fs::read_to_string(fixture.join("master.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let port = destination.local_addr().unwrap().port();
        spawn_reverse_forward(&inst, "test", &target, port).unwrap();
        let pid = master_pid();
        assert_eq!(
            fs::read_to_string(fwd_pid_path(&inst, "test")).unwrap(),
            pid.to_string()
        );
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);

        // Send through the guest listener and receive at the host destination.
        // This also proves the authenticated master survives socket removal.
        let mut sender = Command::new("ip")
            .args(["netns", "exec", "guest", "python3", "-c"])
            .arg("import socket,sys; s=socket.create_connection(('127.0.0.1',int(sys.argv[1])),timeout=5); s.sendall(b'forwarded'); assert s.recv(2)==b'ok'")
            .arg(port.to_string())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut connection = loop {
            match destination.accept() {
                Ok((connection, _)) => break Ok(connection),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "forwarded traffic never arrived");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => break Err(error),
            }
        }
        .unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut payload = [0; 9];
        connection.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"forwarded");
        connection.write_all(b"ok").unwrap();
        assert!(sender.wait().unwrap().success());
        kill_pid_file(&fwd_pid_path(&inst, "test"), "test tunnel");
        assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) }, pid);

        // Occupy exactly the guest loopback port. Authentication still works;
        // only the subsequent reverse-forward bind must be refused.
        let ready = fixture.join("occupied");
        let mut blocker = Command::new("ip")
            .args(["netns", "exec", "guest", "python3", "-c"])
            .arg("import socket,sys,pathlib,time; s=socket.socket(); s.bind(('127.0.0.1',int(sys.argv[1]))); s.listen(); pathlib.Path(sys.argv[2]).touch(); time.sleep(30)")
            .arg(port.to_string())
            .arg(&ready)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(blocker.try_wait().unwrap().is_none(), "bind blocker exited");
            assert!(Instant::now() < deadline, "bind blocker never became ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        let result = spawn_reverse_forward(&inst, "test", &target, port);
        let rejected_pid = master_pid();
        assert!(
            result.is_err(),
            "authenticated reverse bind refusal must fail startup"
        );
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("request failed"), "{error:#}");
        assert!(!fwd_pid_path(&inst, "test").exists());
        assert_eq!(unsafe { libc::kill(rejected_pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        blocker.kill().unwrap();
        blocker.wait().unwrap();
    }

    #[test]
    fn token_file_round_trips_and_clears() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut inst = inst_with_index(0);
        inst.dir = tmp.path().to_path_buf();

        assert_eq!(read_capability_token(&inst, Provider::Openai), None);
        write_token_file(&inst, Provider::Openai, "cap-123").unwrap();
        assert_eq!(
            read_capability_token(&inst, Provider::Openai),
            Some("cap-123".to_string())
        );
        // Other providers are independent.
        assert_eq!(read_capability_token(&inst, Provider::Anthropic), None);

        stop_provider(&inst, Provider::Openai);
        assert_eq!(read_capability_token(&inst, Provider::Openai), None);
    }

    #[test]
    fn read_capability_token_ignores_empty_and_whitespace_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut inst = inst_with_index(0);
        inst.dir = tmp.path().to_path_buf();

        // Empty file → no token (kills a `!token.is_empty()` → `true` mutant,
        // which would otherwise return `Some("")`).
        write_token_file(&inst, Provider::Openai, "").unwrap();
        assert_eq!(read_capability_token(&inst, Provider::Openai), None);

        // Whitespace-only file trims to empty and is likewise ignored.
        write_token_file(&inst, Provider::Openai, "  \n\t ").unwrap();
        assert_eq!(read_capability_token(&inst, Provider::Openai), None);

        // Surrounding whitespace is stripped from a real token (kills a
        // `.trim()`-deletion mutant, invisible to the round-trip test).
        write_token_file(&inst, Provider::Openai, "  cap-123\n").unwrap();
        assert_eq!(
            read_capability_token(&inst, Provider::Openai),
            Some("cap-123".to_string())
        );
    }
}
