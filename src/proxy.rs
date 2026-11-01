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
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::backend::SshTarget;
use crate::codex_token::CodexTokenMetadata;
use crate::config::{
    Instance, InstanceIndex, ProxyAuthScheme, ProxyUpstream, Secret, resolve_cmd_value,
};
use crate::process_identity::HelperRole;
use crate::remote_command::RemoteCommand;

/// The proxy binary name, expected next to the `coop` binary.
const PROXY_BIN_NAME: &str = "coop-proxy";
/// Wire contract mirrored by coop-proxy's bounded startup reader.
const STARTUP_CONFIG_MAX_BYTES: usize = 65_536;
const DIAGNOSTIC_MAX_BYTES: usize = 16_384;

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
    /// `OpenAI` (Codex) → `api.openai.com`, or `chatgpt.com` for workspace PATs.
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

    /// The default fixed upstream. Workspace PAT startup selects `chatgpt.com`
    /// instead; neither destination is guest-controlled (closes SSRF).
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
        self.port_for_index(inst.index)
    }

    fn port_for_index(self, index: InstanceIndex) -> u16 {
        self.base_port().saturating_add(index.as_u16())
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

#[cfg(kani)]
mod proofs {
    use super::{InstanceIndex, Provider};

    /// Every valid provider/instance pair gets an exact, unique host port.
    #[kani::proof]
    fn proxy_ports_are_exact_and_unique() {
        let Some(first_index) = InstanceIndex::new(kani::any()) else {
            return;
        };
        let Some(second_index) = InstanceIndex::new(kani::any()) else {
            return;
        };
        let first = if kani::any() {
            Provider::Anthropic
        } else {
            Provider::Openai
        };
        let second = if kani::any() {
            Provider::Anthropic
        } else {
            Provider::Openai
        };
        let first_port = first.port_for_index(first_index);
        let second_port = second.port_for_index(second_index);
        assert_eq!(
            u32::from(first_port),
            u32::from(first.base_port()) + first_index.as_u32()
        );
        assert_eq!(
            u32::from(second_port),
            u32::from(second.base_port()) + second_index.as_u32()
        );
        if first != second || first_index != second_index {
            assert_ne!(first_port, second_port);
        }
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
    start_provider_with(
        inst,
        provider,
        upstream,
        crate::codex_token::load_metadata,
        |listen, json| spawn_proxy(inst, provider.name(), listen, json),
        |port| spawn_reverse_forward(inst, provider.name(), target, port),
    )
}

// Keep startup composition shared while tests replace only external metadata
// and process boundaries; credential resolution and state cleanup stay real.
fn start_provider_with(
    inst: &Instance,
    provider: Provider,
    upstream: &ProxyUpstream,
    load_metadata: impl FnOnce(&str) -> Result<CodexTokenMetadata>,
    launch_proxy: impl FnOnce(SocketAddr, &str) -> Result<()>,
    launch_tunnel: impl FnOnce(u16) -> Result<()>,
) -> Result<ProxyHandle> {
    ensure_auth_supported(provider, upstream.auth)?;
    // A failed replacement must not leave the previous credential usable.
    stop_provider(inst, provider)?;
    let credential = resolve_cmd_value(upstream.credential.expose()).with_context(|| {
        format!(
            "Failed to resolve the {} proxy credential — aborting VM start (fail-closed); \
             the guest must never come up without the injected credential",
            provider.name()
        )
    })?;

    let metadata = if upstream.auth == ProxyAuthScheme::CodexAccessToken {
        Some(load_metadata(&credential)?)
    } else {
        None
    };
    let token = mint_capability_token()?;
    let port = provider.port(inst);
    let listen = SocketAddr::from(([127, 0, 0, 1], port));
    let json = wire_config_json(
        &listen,
        &token,
        if metadata.is_some() {
            "chatgpt.com"
        } else {
            provider.upstream_host()
        },
        upstream.auth,
        &credential,
        metadata.as_ref(),
    )?;

    launch_proxy(listen, &json)?;
    // Persist the capability token for providers that forward it via env on
    // later sessions (Codex); Claude reads its token from the returned handle
    // (settings.json) and keeps it out of host disk. Written after spawn so a
    // stale token is never left pointing at a dead proxy.
    if provider.persists_token()
        && let Err(e) = write_token_file(inst, provider, &token)
    {
        if let Err(cleanup) = stop_provider(inst, provider) {
            return Err(e).context(format!(
                "Proxy token persistence failed and helper cleanup also failed: {cleanup:#}"
            ));
        }
        return Err(e);
    }
    // Expose the loopback proxy into the guest. If the tunnel fails, tear the
    // proxy back down so we don't leave it orphaned (fail closed).
    if let Err(e) = launch_tunnel(port) {
        if let Err(cleanup) = stop_provider(inst, provider) {
            return Err(e).context(format!(
                "Reverse-tunnel startup failed and helper cleanup also failed: {cleanup:#}"
            ));
        }
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
/// Missing state is already stopped; unverifiable identity fails closed and
/// retains state and token material for a safe retry.
pub fn stop_provider(inst: &Instance, provider: Provider) -> Result<()> {
    let name = provider.name();
    let proxy =
        crate::process_identity::stop(&pid_path(inst, name), HelperRole::CredentialProxy, name);
    let tunnel =
        crate::process_identity::stop(&fwd_pid_path(inst, name), HelperRole::ReverseTunnel, name);
    match (proxy, tunnel) {
        (Ok(()), Ok(())) => {}
        (Err(proxy), Ok(())) => return Err(proxy),
        (Ok(()), Err(tunnel)) => return Err(tunnel),
        (Err(proxy), Err(tunnel)) => {
            return Err(proxy).context(format!(
                "Both {} helper cleanups failed; tunnel cleanup: {tunnel:#}",
                provider.name()
            ));
        }
    }
    let token = token_path(inst, name);
    match fs::remove_file(&token) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to remove proxy token {}", token.display()));
        }
    }
    Ok(())
}

/// Tear down every provider's proxy for `inst` (stop/destroy).
pub fn stop(inst: &Instance) -> Result<()> {
    let mut failures = Vec::new();
    for provider in Provider::ALL {
        if let Err(error) = stop_provider(inst, provider) {
            failures.push(format!("{}: {error:#}", provider.name()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("Credential proxy cleanup failed:\n{}", failures.join("\n"))
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

/// Confirm that a configured provider's host proxy and reverse SSH tunnel are
/// still available before launching an agent that has no public-network
/// fallback. PID checks catch terminated children, the host-loopback connect
/// checks the fixed proxy endpoint, and a guest-loopback request checks the
/// reverse-forward path over the instance's persisted SSH transport.
pub(crate) fn ensure_runtime_available(
    inst: &Instance,
    provider: Provider,
    target: &SshTarget,
) -> Result<()> {
    crate::process_identity::ensure_live(
        &pid_path(inst, provider.name()),
        HelperRole::CredentialProxy,
        provider.name(),
    )?;
    crate::process_identity::ensure_live(
        &fwd_pid_path(inst, provider.name()),
        HelperRole::ReverseTunnel,
        provider.name(),
    )?;
    if provider.persists_token() {
        read_capability_token(inst, provider).with_context(|| {
            format!(
                "{} credential proxy capability token is missing or empty",
                provider.name()
            )
        })?;
    }
    let address = SocketAddr::from(([127, 0, 0, 1], provider.port(inst)));
    ensure_proxy_protocol(address, provider)?;
    let guest_url = format!("http://127.0.0.1:{}/", provider.port(inst));
    let guest_reachable = target.exec_ok(guest_proxy_probe_command(&guest_url))?;
    anyhow::ensure!(
        guest_reachable,
        "{} credential proxy reverse endpoint did not return the expected response on guest loopback",
        provider.name()
    );
    Ok(())
}

fn guest_proxy_probe_command(url: &str) -> RemoteCommand {
    RemoteCommand::new()
        .literal("response=$(curl -q --silent --show-error --noproxy '*' --max-time 3 --write-out ':%{http_code}' ")
        .arg(url)
        .literal(
            ") || exit 1; case \"$response\" in \
             *'missing or invalid coop-proxy capability token'*':401') exit 0 ;; \
             *) exit 1 ;; esac",
        )
}

fn ensure_proxy_protocol(address: SocketAddr, provider: Provider) -> Result<()> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500))
        .with_context(|| {
            format!(
                "{} credential proxy is not accepting connections on host loopback at {address}",
                provider.name()
            )
        })?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")?;
    stream.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    stream.take(4096).read_to_string(&mut response)?;
    anyhow::ensure!(
        response.starts_with("HTTP/1.1 401 ")
            && response.contains("missing or invalid coop-proxy capability token"),
        "{} credential proxy endpoint returned an unexpected protocol response",
        provider.name()
    );
    Ok(())
}

// ── process supervision ──────────────────────────────────────

fn spawn_proxy(inst: &Instance, name: &str, listen: SocketAddr, json: &str) -> Result<()> {
    // A previous run may have crashed without stop cleanup; kill any leftover
    // before binding so we don't race it for the same port.
    crate::process_identity::stop(&pid_path(inst, name), HelperRole::CredentialProxy, name)?;

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
        kill_spawned_group(&mut child);
        return Err(e);
    }

    let pid = child.id();
    let pid_path = pid_path(inst, name);
    if let Err(e) =
        crate::process_identity::write_state(&pid_path, HelperRole::CredentialProxy, name, pid)
    {
        // Without identity state `stop_provider` can never safely authorize
        // teardown. Kill the freshly spawned group before failing.
        kill_spawned_group(&mut child);
        return Err(e)
            .with_context(|| format!("Failed to write proxy state {}", pid_path.display()));
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
        match child_exited_without_reaping(child) {
            Ok(true) => {
                let log = bounded_log_excerpt(log_path);
                bail!(
                    "credential proxy exited before it began serving — its \
                     confinement or bind failed; refusing to start the VM (fail-closed).\n{}",
                    log.trim()
                );
            }
            Ok(false) => {}
            Err(e) => return Err(e).context("Failed to poll the credential proxy process"),
        }
        if std::net::TcpStream::connect_timeout(&listen, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let log = bounded_log_excerpt(log_path);
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
/// On Linux the proxy self-confines with Landlock unconditionally and the
/// binary has no unconfined serving mode, so the command is the binary itself.
/// On macOS — which has no first-class in-process sandbox — the
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

fn reverse_forward_master_args(control_path: &Path, target: &SshTarget) -> Result<Vec<String>> {
    Ok(vec![
        "-N".into(),
        "-T".into(),
        "-o".into(),
        "ControlMaster=yes".into(),
        "-o".into(),
        "ControlPersist=no".into(),
        "-o".into(),
        "ForkAfterAuthentication=no".into(),
        "-S".into(),
        crate::openssh::literal_path(control_path, "proxy SSH control socket path")?,
        target.addr(),
    ])
}

fn reverse_forward_request_args(
    control_path: &Path,
    target: &SshTarget,
    port: u16,
) -> Result<Vec<String>> {
    Ok(vec![
        "-S".into(),
        crate::openssh::literal_path(control_path, "proxy SSH control socket path")?,
        "-O".into(),
        "forward".into(),
        "-R".into(),
        format!("127.0.0.1:{port}:127.0.0.1:{port}"),
        target.addr(),
    ])
}

/// Establish a per-instance reverse SSH tunnel so the guest reaches the
/// host-loopback proxy at `127.0.0.1:{port}` (`ssh -R guest → host`). Detached
/// and tracked by an identity-bound state record like the proxy process, so
/// teardown needs no SSH target. An acknowledged forwarding request makes a
/// bind clash on the guest a startup failure rather than a silently dead tunnel.
fn spawn_reverse_forward(inst: &Instance, name: &str, target: &SshTarget, port: u16) -> Result<()> {
    crate::process_identity::stop(&fwd_pid_path(inst, name), HelperRole::ReverseTunnel, name)?;

    let control_dir = create_tunnel_control_dir()?;
    let control_path = control_dir.path().join("ssh.sock");
    let log_path = inst.dir.join(format!("proxy-{name}-fwd.log"));
    let log = File::create(&log_path)
        .with_context(|| format!("Failed to create proxy tunnel log {}", log_path.display()))?;
    let forward_log = log
        .try_clone()
        .context("Failed to clone proxy tunnel log")?;

    let args = reverse_forward_master_args(&control_path, target)?;

    let mut child = target
        .ssh_forward_command()?
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
            if child_exited_without_reaping(&child).context("Failed to poll proxy tunnel")? {
                bail!("reverse SSH tunnel exited before its control socket became ready");
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
        // child so coop can persist its complete identity record after the
        // request succeeds.
        let forward_args = reverse_forward_request_args(&control_path, target, port)?;
        let request = target
            .ssh_forward_command()?
            .args(&forward_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(forward_log))
            .spawn()
            .context("Failed to request the credential proxy's reverse forward")?;
        await_forwarding_ack(request, deadline)?;

        let path = fwd_pid_path(inst, name);
        crate::process_identity::write_state(&path, HelperRole::ReverseTunnel, name, child.id())
            .with_context(|| format!("Failed to write proxy tunnel state {}", path.display()))?;
        Ok(())
    })();
    if let Err(e) = setup {
        kill_spawned_group(&mut child);
        let log = bounded_log_excerpt(&log_path);
        return Err(e).context(format!(
            "Failed to establish the credential proxy's reverse SSH tunnel.\n{}",
            log.trim()
        ));
    }
    // The master no longer needs a control socket: later teardown uses its
    // persisted identity and stable process authority.
    Ok(())
}

fn bounded_log_excerpt(path: &Path) -> String {
    let Ok(file) = File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::with_capacity(DIAGNOSTIC_MAX_BYTES + 1);
    if file
        .take((DIAGNOSTIC_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return String::new();
    }
    let truncated = bytes.len() > DIAGNOSTIC_MAX_BYTES;
    bytes.truncate(DIAGNOSTIC_MAX_BYTES);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str("\n[diagnostic truncated at 16384 bytes]");
    }
    text
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

fn kill_spawned_group(child: &mut std::process::Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // Readiness polling uses waitid(WNOWAIT), so this direct child has not
        // been reaped: a live process or zombie still reserves its PID/PGID.
        // The numeric group cannot be recycled before the wait below.
        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    // The unreaped direct-child invariant also makes this numeric PID stable.
    let _ = child.kill();
    let _ = child.wait();
}

fn child_exited_without_reaping(child: &std::process::Child) -> Result<bool> {
    // SAFETY: siginfo_t is a C output structure initialized before use.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // WNOWAIT observes exit without releasing the PID/PGID authority required
    // by startup rollback. P_PID limits the query to this direct child.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            std::ptr::addr_of_mut!(info),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if waitid_failed(result) {
        return Err(std::io::Error::last_os_error()).context("Failed to observe child process");
    }
    // SAFETY: waitid initialized siginfo; si_pid is zero for WNOHANG with no
    // state change and the child's PID after an observed exit.
    Ok(unsafe { info.si_pid() } != 0)
}

fn waitid_failed(result: libc::c_int) -> bool {
    result == -1
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
    XApiKey {
        credential: &'a str,
    },
    Bearer {
        credential: &'a str,
    },
    CodexAccessToken {
        credential: &'a str,
        account_id: &'a str,
        is_fedramp: bool,
    },
}

fn ensure_auth_supported(provider: Provider, auth: ProxyAuthScheme) -> Result<()> {
    if provider != Provider::Openai && auth == ProxyAuthScheme::CodexAccessToken {
        bail!("codex_access_token auth is supported only by [proxy.openai]");
    }
    Ok(())
}

fn wire_config_json(
    listen: &SocketAddr,
    capability_token: &str,
    upstream_host: &str,
    auth: ProxyAuthScheme,
    credential: &str,
    metadata: Option<&CodexTokenMetadata>,
) -> Result<String> {
    let injection = match auth {
        ProxyAuthScheme::ApiKey => WireInjection::XApiKey { credential },
        ProxyAuthScheme::Bearer => WireInjection::Bearer { credential },
        ProxyAuthScheme::CodexAccessToken => {
            let metadata = metadata.context("Codex proxy requires validated workspace metadata")?;
            WireInjection::CodexAccessToken {
                credential,
                account_id: &metadata.chatgpt_account_id,
                is_fedramp: metadata.chatgpt_account_is_fedramp,
            }
        }
    };
    let wire = WireConfig {
        listen: listen.to_string(),
        capability_token,
        upstream_host,
        injection,
    };
    let json = serde_json::to_string(&wire).context("Failed to serialize proxy startup config")?;
    anyhow::ensure!(
        json.len() <= STARTUP_CONFIG_MAX_BYTES,
        "credential proxy startup config exceeds the {STARTUP_CONFIG_MAX_BYTES}-byte limit"
    );
    Ok(json)
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
            firecracker_control: crate::config::FirecrackerControlMode::LegacyTcp,
        }
    }

    #[cfg(target_os = "linux")]
    fn forward_master_pid(fixture: &Path) -> i32 {
        fs::read_to_string(fixture.join("master.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn ports_are_per_instance_and_per_provider() {
        assert_eq!(Provider::Anthropic.port(&inst_with_index(0)), 8788);
        assert_eq!(Provider::Anthropic.port(&inst_with_index(5)), 8793);
        assert_eq!(Provider::Openai.port(&inst_with_index(0)), 9788);
        assert_eq!(Provider::Openai.port(&inst_with_index(5)), 9793);
    }

    #[test]
    fn reverse_forward_arguments_preserve_tcp_target_and_loopback_binding() {
        let target = SshTarget::tcp(
            crate::backend::Hostname::new("192.0.2.2").unwrap(),
            std::num::NonZeroU16::new(2222).unwrap(),
            crate::backend::SshUser::new("ubuntu").unwrap(),
            PathBuf::from("/tmp/key"),
            crate::config::SshHostToolsConfig::default(),
        );
        let socket = Path::new("/tmp/proxy-control.sock");

        assert_eq!(
            reverse_forward_master_args(socket, &target).unwrap(),
            [
                "-N",
                "-T",
                "-o",
                "ControlMaster=yes",
                "-o",
                "ControlPersist=no",
                "-o",
                "ForkAfterAuthentication=no",
                "-S",
                "/tmp/proxy-control.sock",
                "ubuntu@192.0.2.2",
            ]
        );
        assert_eq!(
            reverse_forward_request_args(socket, &target, 8788).unwrap(),
            [
                "-S",
                "/tmp/proxy-control.sock",
                "-O",
                "forward",
                "-R",
                "127.0.0.1:8788:127.0.0.1:8788",
                "ubuntu@192.0.2.2",
            ]
        );
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
            None,
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
    fn wire_json_accepts_exact_limit_and_rejects_one_byte_over() {
        let listen: SocketAddr = "127.0.0.1:8788".parse().unwrap();
        let empty = wire_config_json(
            &listen,
            "cap",
            "api.anthropic.com",
            ProxyAuthScheme::ApiKey,
            "",
            None,
        )
        .unwrap();
        let exact_credential = "x".repeat(STARTUP_CONFIG_MAX_BYTES - empty.len());
        let exact = wire_config_json(
            &listen,
            "cap",
            "api.anthropic.com",
            ProxyAuthScheme::ApiKey,
            &exact_credential,
            None,
        )
        .unwrap();
        assert_eq!(exact.len(), STARTUP_CONFIG_MAX_BYTES);

        let oversized_credential = format!("{exact_credential}x");
        let error = wire_config_json(
            &listen,
            "cap",
            "api.anthropic.com",
            ProxyAuthScheme::ApiKey,
            &oversized_credential,
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("byte limit"));
    }

    #[test]
    fn proxy_log_excerpt_is_bounded_and_marks_truncation() {
        let temp = crate::test_support::private_tempdir().unwrap();
        let path = temp.path().join("proxy.log");
        fs::write(&path, vec![b'x'; DIAGNOSTIC_MAX_BYTES]).unwrap();
        let exact = bounded_log_excerpt(&path);
        assert_eq!(exact, "x".repeat(DIAGNOSTIC_MAX_BYTES));

        fs::write(&path, vec![b'x'; DIAGNOSTIC_MAX_BYTES + 1]).unwrap();
        let excerpt = bounded_log_excerpt(&path);
        assert!(excerpt.starts_with(&"x".repeat(DIAGNOSTIC_MAX_BYTES)));
        assert!(excerpt.ends_with("[diagnostic truncated at 16384 bytes]"));
        assert!(excerpt.len() < DIAGNOSTIC_MAX_BYTES + 64);
    }

    #[test]
    fn wire_json_codex_token_carries_host_validated_metadata() {
        let metadata = CodexTokenMetadata {
            chatgpt_account_id: "workspace-123".to_string(),
            chatgpt_account_is_fedramp: true,
        };
        let listen = "127.0.0.1:9788".parse().unwrap();
        let json = wire_config_json(
            &listen,
            "cap",
            "chatgpt.com",
            ProxyAuthScheme::CodexAccessToken,
            "at-private",
            Some(&metadata),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["upstream_host"], "chatgpt.com");
        assert_eq!(value["injection"]["scheme"], "codex_access_token");
        assert_eq!(value["injection"]["credential"], "at-private");
        assert_eq!(value["injection"]["account_id"], "workspace-123");
        assert_eq!(value["injection"]["is_fedramp"], true);
        assert!(
            wire_config_json(
                &listen,
                "cap",
                "chatgpt.com",
                ProxyAuthScheme::CodexAccessToken,
                "at-private",
                None
            )
            .is_err()
        );
    }

    #[test]
    fn codex_token_auth_is_openai_only_including_vm_overrides() {
        assert!(ensure_auth_supported(Provider::Openai, ProxyAuthScheme::CodexAccessToken).is_ok());
        assert!(
            ensure_auth_supported(Provider::Anthropic, ProxyAuthScheme::CodexAccessToken).is_err()
        );
        for auth in [ProxyAuthScheme::ApiKey, ProxyAuthScheme::Bearer] {
            assert!(ensure_auth_supported(Provider::Anthropic, auth).is_ok());
            assert!(ensure_auth_supported(Provider::Openai, auth).is_ok());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invalid_codex_token_fails_before_proxy_or_tunnel_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let mut inst = inst_with_index(0);
        inst.dir = tmp.path().to_path_buf();
        let upstream = ProxyUpstream {
            credential: Secret::new("sk-wrong-type".to_string()),
            auth: ProxyAuthScheme::CodexAccessToken,
        };
        let target = SshTarget::tcp(
            crate::backend::Hostname::new("127.0.0.1").unwrap(),
            std::num::NonZeroU16::new(22).unwrap(),
            crate::backend::SshUser::new("ubuntu").unwrap(),
            tmp.path().join("unused_key"),
            crate::config::CoopConfig::default().ssh.host_tools,
        );
        let mut stale = StaleProvider::seed(&inst);
        let error = start_provider(&inst, Provider::Openai, &upstream, &target).unwrap_err();
        stale.assert_stopped(&inst);
        assert!(
            error
                .to_string()
                .contains("Expected a Codex personal access token")
        );
        assert!(!format!("{error:#}").contains("sk-wrong-type"));
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    struct StaleProvider {
        children: [std::process::Child; 2],
    }

    impl StaleProvider {
        fn seed(inst: &Instance) -> Self {
            let children = std::array::from_fn(|_| {
                let mut command = Command::new("sleep");
                command.arg("30").process_group(0).spawn().unwrap()
            });
            for ((path, role), child) in [
                (pid_path(inst, "openai"), HelperRole::CredentialProxy),
                (fwd_pid_path(inst, "openai"), HelperRole::ReverseTunnel),
            ]
            .iter()
            .zip(&children)
            {
                crate::process_identity::write_state(path, *role, "openai", child.id()).unwrap();
            }
            write_token_file(inst, Provider::Openai, "stale-capability").unwrap();
            Self { children }
        }

        #[cfg(target_os = "linux")]
        fn assert_stopped(&mut self, inst: &Instance) {
            for path in [
                pid_path(inst, "openai"),
                fwd_pid_path(inst, "openai"),
                token_path(inst, "openai"),
            ] {
                assert!(!path.exists(), "stale state remains: {}", path.display());
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            for child in &self.children {
                let pid = i32::try_from(child.id()).unwrap();
                while unsafe { libc::kill(pid, 0) } == 0 {
                    assert!(Instant::now() < deadline, "stale process still running");
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
            }
        }

        #[cfg(target_os = "macos")]
        fn assert_retained(&mut self, inst: &Instance) {
            for path in [
                pid_path(inst, "openai"),
                fwd_pid_path(inst, "openai"),
                token_path(inst, "openai"),
            ] {
                assert!(path.exists(), "stale state was removed: {}", path.display());
            }
            for child in &mut self.children {
                assert!(child.try_wait().unwrap().is_none());
            }
        }
    }

    impl Drop for StaleProvider {
        fn drop(&mut self) {
            // Also clean up if a deliberate regression makes an assertion fail.
            for child in &mut self.children {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_codex_start_clears_previous_provider_state() {
        use std::cell::Cell;

        for credential in ["cmd:exit 7", "at-test"] {
            let tmp = tempfile::tempdir().unwrap();
            let mut inst = inst_with_index(0);
            inst.dir = tmp.path().to_path_buf();
            let mut stale = StaleProvider::seed(&inst);
            let upstream = ProxyUpstream {
                credential: Secret::new(credential.to_string()),
                auth: ProxyAuthScheme::CodexAccessToken,
            };
            let metadata_called = Cell::new(false);
            let processes_started = Cell::new(0);
            let error = start_provider_with(
                &inst,
                Provider::Openai,
                &upstream,
                |token| {
                    metadata_called.set(true);
                    assert_eq!(token, "at-test");
                    // Cleanup must already have happened before the lookup.
                    stale.assert_stopped(&inst);
                    bail!("metadata lookup failed")
                },
                |_, _| {
                    processes_started.set(processes_started.get() + 1);
                    Ok(())
                },
                |_| {
                    processes_started.set(processes_started.get() + 1);
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(metadata_called.get(), credential == "at-test");
            if metadata_called.get() {
                assert_eq!(error.to_string(), "metadata lookup failed");
            } else {
                assert!(error.to_string().contains("Failed to resolve"));
            }
            assert_eq!(processes_started.get(), 0);
            stale.assert_stopped(&inst);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn replacement_refuses_live_helpers_without_stable_signal_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let mut inst = inst_with_index(0);
        inst.dir = tmp.path().to_path_buf();
        let mut stale = StaleProvider::seed(&inst);
        let upstream = ProxyUpstream {
            credential: Secret::new("at-test".to_string()),
            auth: ProxyAuthScheme::CodexAccessToken,
        };
        let target = SshTarget::tcp(
            crate::backend::Hostname::new("127.0.0.1").unwrap(),
            std::num::NonZeroU16::new(22).unwrap(),
            crate::backend::SshUser::new("ubuntu").unwrap(),
            tmp.path().join("unused_key"),
            crate::config::CoopConfig::default().ssh.host_tools,
        );

        let error = start_provider(&inst, Provider::Openai, &upstream, &target).unwrap_err();
        assert!(format!("{error:#}").contains("race-free helper signal authority"));
        stale.assert_retained(&inst);
    }

    #[test]
    fn codex_start_selects_metadata_upstream_and_persists_capability() {
        use std::cell::{Cell, RefCell};

        let tmp = tempfile::tempdir().unwrap();
        let mut inst = inst_with_index(3);
        inst.dir = tmp.path().to_path_buf();
        let upstream = ProxyUpstream {
            credential: Secret::new("cmd:printf at-test".to_string()),
            auth: ProxyAuthScheme::CodexAccessToken,
        };
        let captured = RefCell::new(None);
        let tunnel_started = Cell::new(false);
        let handle = start_provider_with(
            &inst,
            Provider::Openai,
            &upstream,
            |token| {
                assert_eq!(token, "at-test");
                Ok(CodexTokenMetadata {
                    chatgpt_account_id: "workspace-test".to_string(),
                    chatgpt_account_is_fedramp: true,
                })
            },
            |listen, json| {
                assert_eq!(listen, "127.0.0.1:9791".parse::<SocketAddr>().unwrap());
                let config: serde_json::Value = serde_json::from_str(json).unwrap();
                assert_eq!(config["listen"], listen.to_string());
                assert_eq!(config["upstream_host"], "chatgpt.com");
                assert_eq!(config["injection"]["scheme"], "codex_access_token");
                assert_eq!(config["injection"]["credential"], "at-test");
                assert_eq!(config["injection"]["account_id"], "workspace-test");
                assert_eq!(config["injection"]["is_fedramp"], true);
                *captured.borrow_mut() = Some(config);
                Ok(())
            },
            |port| {
                assert_eq!(port, 9791);
                assert!(captured.borrow().is_some());
                assert!(read_capability_token(&inst, Provider::Openai).is_some());
                tunnel_started.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(tunnel_started.get());
        assert_eq!(handle.base_url, "http://127.0.0.1:9791");
        let token = read_capability_token(&inst, Provider::Openai).unwrap();
        assert_eq!(&token, handle.capability_token.expose());
        assert_eq!(token.len(), 64);
        assert_eq!(captured.into_inner().unwrap()["capability_token"], token);
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
            None,
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
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(7), "exit observation must not reap");
    }

    #[test]
    fn child_exit_probe_distinguishes_live_process_without_reaping() {
        let mut live = Command::new("sleep").arg("30").spawn().unwrap();
        assert!(!child_exited_without_reaping(&live).unwrap());
        live.kill().unwrap();
        live.wait().unwrap();

        let mut exited = Command::new("sh").args(["-c", "exit 9"]).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !child_exited_without_reaping(&exited).unwrap() {
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(exited.wait().unwrap().code(), Some(9));
    }

    #[test]
    fn waitid_failure_classifier_distinguishes_error_from_success() {
        assert!(waitid_failed(-1));
        assert!(!waitid_failed(0));
    }

    #[test]
    fn startup_rollback_kills_and_reaps_the_reserved_group_leader() {
        let temp = tempfile::TempDir::new().unwrap();
        let descendant_state = temp.path().join("descendant-pid");
        let mut command = Command::new("sh");
        let mut child = command
            .args([
                "-c",
                "sh -c 'echo $$ > \"$1\"; exec sleep 30' sh \"$1\" & wait",
                "coop-test",
                descendant_state.to_str().unwrap(),
            ])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !descendant_state.exists() {
            assert!(Instant::now() < deadline, "descendant did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        let descendant = fs::read_to_string(&descendant_state)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert!(!child_exited_without_reaping(&child).unwrap());

        kill_spawned_group(&mut child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(descendant, 0) } == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let leader_alive = unsafe { libc::kill(pid, 0) } == 0;
        let descendant_alive = unsafe { libc::kill(descendant, 0) } == 0;
        if leader_alive || descendant_alive {
            // Cleanup also runs if a deliberate mutation removes rollback.
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(!leader_alive, "rollback left the group leader alive");
        assert!(!descendant_alive, "rollback left a group descendant alive");
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
        let target = SshTarget::tcp(
            crate::backend::Hostname::new("127.0.0.1").unwrap(),
            std::num::NonZeroU16::new(port).unwrap(),
            crate::backend::SshUser::new("coop").unwrap(),
            tmp.path().join("unused-key"),
            crate::config::SshHostToolsConfig::default(),
        );
        let result = spawn_reverse_forward(&inst, "test", &target, 8788);
        let accepted = peer.join().unwrap().unwrap();
        // Also clean up the old implementation when deliberately regressed.
        crate::process_identity::stop(
            &fwd_pid_path(&inst, "test"),
            HelperRole::ReverseTunnel,
            "test",
        )
        .unwrap();
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
        let target = SshTarget::tcp(
            crate::backend::Hostname::new("192.0.2.2").unwrap(),
            std::num::NonZeroU16::new(2222).unwrap(),
            crate::backend::SshUser::new("root").unwrap(),
            fixture.join("client"),
            crate::config::SshHostToolsConfig::with_exact_paths(
                Some(&fixture.join("bin/ssh")),
                None,
                None,
            )
            .unwrap(),
        );
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let port = destination.local_addr().unwrap().port();
        spawn_reverse_forward(&inst, "test", &target, port).unwrap();
        let ambient_port = std::env::var("COOP_FORWARD_TEST_AMBIENT_PORT")
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let ambient_addr = format!("192.0.2.2:{ambient_port}").parse().unwrap();
        assert!(
            std::net::TcpStream::connect_timeout(&ambient_addr, Duration::from_millis(200))
                .is_err(),
            "ambient SSH configuration added an unexpected remote forward"
        );
        let pid = forward_master_pid(&fixture);
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(fwd_pid_path(&inst, "test")).unwrap()).unwrap();
        assert_eq!(state["pid"], pid);
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
        crate::process_identity::stop(
            &fwd_pid_path(&inst, "test"),
            HelperRole::ReverseTunnel,
            "test",
        )
        .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);

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
        let rejected_pid = forward_master_pid(&fixture);
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

        stop_provider(&inst, Provider::Openai).unwrap();
        assert_eq!(read_capability_token(&inst, Provider::Openai), None);
    }

    #[test]
    fn identity_mismatch_retains_token_and_helper_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut inst = inst_with_index(0);
        inst.dir = tmp.path().to_path_buf();
        let mut command = Command::new("sleep");
        let mut child = command.arg("30").process_group(0).spawn().unwrap();
        let state_path = pid_path(&inst, "openai");
        crate::process_identity::write_state(
            &state_path,
            HelperRole::CredentialProxy,
            "openai",
            child.id(),
        )
        .unwrap();
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state["start_id"] = serde_json::Value::String("recycled".to_string());
        crate::fs_util::atomic_write_with_mode(
            &state_path,
            &serde_json::to_string(&state).unwrap(),
            0o600,
        )
        .unwrap();
        write_token_file(&inst, Provider::Openai, "cap-123").unwrap();

        assert!(stop_provider(&inst, Provider::Openai).is_err());
        assert!(state_path.exists());
        assert_eq!(
            read_capability_token(&inst, Provider::Openai),
            Some("cap-123".to_string())
        );
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
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

    #[test]
    fn protocol_probe_requires_the_proxy_unauthorized_response() {
        fn serve_once(response: &'static [u8]) -> SocketAddr {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 512];
                let _ = stream.read(&mut request).unwrap();
                stream.write_all(response).unwrap();
            });
            address
        }

        let expected = serve_once(
            b"HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\nmissing or invalid coop-proxy capability token",
        );
        ensure_proxy_protocol(expected, Provider::Openai).unwrap();

        let unrelated =
            serve_once(b"HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\nunrelated listener");
        assert!(ensure_proxy_protocol(unrelated, Provider::Openai).is_err());
    }

    #[test]
    fn guest_protocol_probe_requires_the_proxy_unauthorized_response() {
        let curl_home = crate::test_support::private_tempdir().unwrap();
        std::fs::write(curl_home.path().join(".curlrc"), "fail\n").unwrap();
        let accepts = |status: &str, body: &str| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let server = std::thread::spawn(move || -> std::io::Result<()> {
                listener.set_nonblocking(true)?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "probe did not connect to the fixture",
                                ));
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) => return Err(error),
                    }
                };
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(std::time::Duration::from_secs(4)))?;
                let mut request = [0_u8; 512];
                let _ = stream.read(&mut request)?;
                stream.write_all(response.as_bytes())?;
                Ok(())
            });
            let probe = super::guest_proxy_probe_command(&url).into_string();
            let accepted = std::process::Command::new("sh")
                .arg("-c")
                .arg(&probe)
                .env("CURL_HOME", curl_home.path())
                .env("HOME", curl_home.path())
                .status()
                .unwrap()
                .success();
            server.join().unwrap().unwrap();
            accepted
        };
        assert!(accepts(
            "401",
            "missing or invalid coop-proxy capability token"
        ));
        for (status, body) in [
            ("502", "upstream request failed"),
            ("401", "unrelated listener"),
            ("500", "missing or invalid coop-proxy capability token"),
            ("200", "missing or invalid coop-proxy capability token"),
        ] {
            assert!(
                !accepts(status, body),
                "unexpectedly accepted {status} {body}"
            );
        }
    }
}
