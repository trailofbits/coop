//! Host-side lifecycle for the credential-injecting proxy (issue #411).
//!
//! `coop-proxy` is a separate binary (its own workspace crate) that runs on
//! the host for the lifetime of a remote-mode VM. This module resolves the
//! real credential, mints a per-instance capability token, spawns the proxy
//! process (bound on host loopback), and exposes it into the guest with a
//! per-instance `ssh -R` reverse tunnel — both tracked by PID files.
//!
//! Binding host loopback + reverse-tunnelling works identically on both
//! backends (Firecracker and Lima), keeps the listener off every non-loopback
//! interface, and gives each guest its own tunnel (no shared-bridge exposure).
//! The guest is pointed at `http://127.0.0.1:<port>` and holds only the
//! capability token, which the proxy verifies before injecting the real
//! credential upstream. See [`docs/design/issue-411-injecting-proxy.md`].

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::backend::SshTarget;
use crate::config::{Instance, ProxyAuthScheme, ProxyConfig, resolve_cmd_value};

/// The fixed Anthropic upstream. The guest cannot influence this — only the
/// request path is forwarded (closes SSRF).
const ANTHROPIC_UPSTREAM_HOST: &str = "api.anthropic.com";

/// Base port for the per-instance Anthropic proxy. The actual port is
/// `BASE + instance index`, so concurrent VMs never collide on host loopback.
/// The same number is used on the guest's loopback via the reverse tunnel.
const ANTHROPIC_BASE_PORT: u16 = 8788;

/// The proxy binary name, expected next to the `coop` binary.
const PROXY_BIN_NAME: &str = "coop-proxy";

/// A running Anthropic proxy and the values the guest config needs.
#[derive(Debug, Clone)]
pub struct AnthropicProxy {
    /// Base URL the guest is pointed at, e.g. `http://127.0.0.1:8788`.
    pub base_url: String,
    /// Per-instance capability token the guest presents (as
    /// `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer`).
    pub capability_token: String,
}

/// The set of proxies started for one instance. v1: Anthropic only.
#[derive(Debug, Clone, Default)]
pub struct RunningProxies {
    pub anthropic: Option<AnthropicProxy>,
}

/// Start the configured proxies for `inst`: bind the proxy on host loopback
/// and open a reverse SSH tunnel via `target` so the guest reaches it at
/// `127.0.0.1:<port>`. Returns `Ok(None)` when proxy mode is not configured.
///
/// **Fail closed:** if the credential cannot be resolved (or the tunnel cannot
/// be established), this returns an error and the VM start is aborted — the
/// guest never comes up on a path where the agent silently has no or the wrong
/// credential.
pub fn start(
    inst: &Instance,
    cfg: &ProxyConfig,
    target: &SshTarget,
) -> Result<Option<RunningProxies>> {
    let Some(anthropic_cfg) = &cfg.anthropic else {
        return Ok(None);
    };

    let credential = resolve_cmd_value(anthropic_cfg.credential.expose()).context(
        "Failed to resolve the Anthropic proxy credential — aborting VM start (fail-closed); \
         the guest must never come up without the injected credential",
    )?;

    let token = mint_capability_token()?;
    let port = anthropic_port(inst);
    let listen = SocketAddr::from(([127, 0, 0, 1], port));
    let json = wire_config_json(
        &listen,
        &token,
        ANTHROPIC_UPSTREAM_HOST,
        anthropic_cfg.auth,
        &credential,
    )?;

    spawn_proxy(inst, "anthropic", &json)?;
    // Expose the loopback proxy into the guest. If the tunnel fails, tear the
    // proxy back down so we don't leave it orphaned (fail closed).
    if let Err(e) = spawn_reverse_forward(inst, "anthropic", target, port) {
        stop(inst);
        return Err(e);
    }
    tracing::info!("Started Anthropic credential proxy on {listen} (guest → 127.0.0.1:{port})");

    Ok(Some(RunningProxies {
        anthropic: Some(AnthropicProxy {
            base_url: format!("http://127.0.0.1:{port}"),
            capability_token: token,
        }),
    }))
}

/// Tear down every proxy process and tunnel for `inst`. Best-effort, safe to
/// call when none were started (mirrors `teardown_ssh_forwards`).
pub fn stop(inst: &Instance) {
    kill_pid_file(&pid_path(inst, "anthropic"), "anthropic proxy");
    kill_pid_file(&fwd_pid_path(inst, "anthropic"), "anthropic proxy tunnel");
}

// ── process supervision ──────────────────────────────────────

fn spawn_proxy(inst: &Instance, name: &str, json: &str) -> Result<()> {
    // A previous run may have crashed without stop cleanup; kill any leftover
    // before binding so we don't race it for the same port.
    kill_pid_file(&pid_path(inst, name), "stale proxy");

    let bin = locate_proxy_binary()?;
    let log_path = inst.dir.join(format!("proxy-{name}.log"));
    let log = File::create(&log_path)
        .with_context(|| format!("Failed to create proxy log {}", log_path.display()))?;

    let mut child = Command::new(&bin)
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

    let pid = child.id();
    let pid_path = pid_path(inst, name);
    if let Err(e) = fs::write(&pid_path, pid.to_string()) {
        // Without a pid file `stop_one` can never reap this proxy, and the std
        // `Child` destructor does not kill it — so a running proxy holding the
        // real credential would be orphaned. Kill it before failing.
        let _ = child.kill();
        let _ = child.wait();
        return Err(e)
            .with_context(|| format!("Failed to write proxy pid file {}", pid_path.display()));
    }
    // Deliberately do not wait past this point: the proxy runs for the VM's
    // lifetime, and the std `Child` destructor does not kill it.
    Ok(())
}

/// Establish a per-instance reverse SSH tunnel so the guest reaches the
/// host-loopback proxy at `127.0.0.1:{port}` (`ssh -R guest → host`). Detached
/// and tracked by a PID file like the proxy process, so teardown needs no SSH
/// target. `ExitOnForwardFailure` makes a bind clash on the guest a loud
/// failure rather than a silently dead tunnel.
fn spawn_reverse_forward(inst: &Instance, name: &str, target: &SshTarget, port: u16) -> Result<()> {
    kill_pid_file(&fwd_pid_path(inst, name), "stale proxy tunnel");

    let log_path = inst.dir.join(format!("proxy-{name}-fwd.log"));
    let log = File::create(&log_path)
        .with_context(|| format!("Failed to create proxy tunnel log {}", log_path.display()))?;

    let mut args = target.ssh_opts();
    args.extend([
        "-N".into(),
        "-T".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-R".into(),
        format!("127.0.0.1:{port}:127.0.0.1:{port}"),
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

    let pid = child.id();
    let path = fwd_pid_path(inst, name);
    if let Err(e) = fs::write(&path, pid.to_string()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e)
            .with_context(|| format!("Failed to write proxy tunnel pid file {}", path.display()));
    }
    Ok(())
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

fn pid_path(inst: &Instance, name: &str) -> PathBuf {
    inst.dir.join(format!("proxy-{name}.pid"))
}

fn fwd_pid_path(inst: &Instance, name: &str) -> PathBuf {
    inst.dir.join(format!("proxy-{name}-fwd.pid"))
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

/// Per-instance listen port: base + index, so concurrent VMs never collide.
fn anthropic_port(inst: &Instance) -> u16 {
    ANTHROPIC_BASE_PORT.saturating_add(inst.index.as_u16())
}

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
    fn anthropic_port_is_per_instance() {
        assert_eq!(anthropic_port(&inst_with_index(0)), 8788);
        assert_eq!(anthropic_port(&inst_with_index(5)), 8793);
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
        let listen: SocketAddr = "172.16.0.1:8788".parse().unwrap();
        let json = wire_config_json(
            &listen,
            "t",
            "api.anthropic.com",
            ProxyAuthScheme::Bearer,
            "setup-tok",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["injection"]["scheme"], "bearer");
        assert_eq!(v["injection"]["credential"], "setup-tok");
    }
}
