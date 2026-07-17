//! Host-side lifecycle for the credential-injecting proxy (issue #411).
//!
//! `coop-proxy` is a separate binary (its own workspace crate) that runs on
//! the host for the lifetime of a remote-mode VM. This module resolves the
//! real credential, mints a per-instance capability token, and spawns/​tears
//! down the proxy process — the same shape as the `port_forward` SSH
//! supervision, but for our own binary tracked by a PID file.
//!
//! The guest never receives the real credential: it is pointed at
//! `http://<gateway>:<port>` and holds only the capability token, which the
//! proxy verifies before injecting the real credential upstream. See
//! [`docs/design/issue-411-injecting-proxy.md`].

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::{Instance, ProxyAuthScheme, ProxyConfig, resolve_cmd_value};

/// The fixed Anthropic upstream. The guest cannot influence this — only the
/// request path is forwarded (closes SSRF).
const ANTHROPIC_UPSTREAM_HOST: &str = "api.anthropic.com";

/// Base host port for the per-instance Anthropic proxy. The actual port is
/// `BASE + instance index`, so concurrent VMs sharing the bridge gateway IP
/// never collide (mirrors the per-instance `guest_ip` scheme).
const ANTHROPIC_BASE_PORT: u16 = 8788;

/// The proxy binary name, expected next to the `coop` binary.
const PROXY_BIN_NAME: &str = "coop-proxy";

/// A running Anthropic proxy and the values the guest config needs.
#[derive(Debug, Clone)]
pub struct AnthropicProxy {
    /// Base URL the guest is pointed at, e.g. `http://172.16.0.1:8788`.
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

/// Start the configured proxies for `inst`, binding on `bind_ip` (the
/// backend's guest-visible gateway address). Returns `Ok(None)` when proxy
/// mode is not configured.
///
/// **Fail closed:** if the credential cannot be resolved, this returns an
/// error and the VM start is aborted — the guest never comes up on a path
/// where the agent silently has no or the wrong credential.
pub fn start(
    inst: &Instance,
    cfg: &ProxyConfig,
    bind_ip: Ipv4Addr,
) -> Result<Option<RunningProxies>> {
    let Some(anthropic_cfg) = &cfg.anthropic else {
        return Ok(None);
    };

    let credential = resolve_cmd_value(anthropic_cfg.credential.expose()).context(
        "Failed to resolve the Anthropic proxy credential — aborting VM start (fail-closed); \
         the guest must never come up without the injected credential",
    )?;

    let token = mint_capability_token()?;
    let listen = SocketAddr::from((bind_ip, anthropic_port(inst)));
    let json = wire_config_json(
        &listen,
        &token,
        ANTHROPIC_UPSTREAM_HOST,
        anthropic_cfg.auth,
        &credential,
    )?;

    spawn_proxy(inst, "anthropic", &json)?;
    tracing::info!("Started Anthropic credential proxy on {listen}");

    Ok(Some(RunningProxies {
        anthropic: Some(AnthropicProxy {
            base_url: format!("http://{listen}"),
            capability_token: token,
        }),
    }))
}

/// Tear down every proxy process for `inst`. Best-effort, safe to call when
/// none were started (mirrors `teardown_ssh_forwards`).
pub fn stop(inst: &Instance) {
    stop_one(inst, "anthropic");
}

// ── process supervision ──────────────────────────────────────

fn spawn_proxy(inst: &Instance, name: &str, json: &str) -> Result<()> {
    // A previous run may have crashed without stop cleanup; kill any leftover
    // before binding so we don't race it for the same port.
    stop_one(inst, name);

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

fn stop_one(inst: &Instance, name: &str) {
    let path = pid_path(inst, name);
    let Ok(contents) = fs::read_to_string(&path) else {
        return;
    };
    match contents.trim().parse::<i32>() {
        Ok(pid) if pid > 0 => {
            // SIGTERM → the proxy stops accepting and exits cleanly.
            // Safety: `kill` with a parsed pid and a constant signal.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            tracing::debug!("Sent SIGTERM to {name} proxy (pid {pid})");
        }
        _ => tracing::debug!("Ignoring malformed proxy pid file {}", path.display()),
    }
    if let Err(e) = fs::remove_file(&path) {
        tracing::debug!(
            "Failed to remove proxy pid file {} (non-fatal): {e}",
            path.display()
        );
    }
}

fn pid_path(inst: &Instance, name: &str) -> PathBuf {
    inst.dir.join(format!("proxy-{name}.pid"))
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
