//! `coop-proxy` — the host-side credential-injecting reverse proxy.
//!
//! `coop` spawns one process per proxied upstream (per-instance, per-agent),
//! writes the startup config to its stdin (see [`config`]), and supervises it
//! for the lifetime of the VM. The guest is pointed at this process via a
//! base-URL override and holds only a per-instance capability token; the real
//! upstream credential lives here, in host memory, and is attached to
//! outbound requests the guest never sees.

mod config;
mod jail;
mod proxy;
mod tls;

use std::ffi::{OsStr, OsString};
use std::io::Read;

use anyhow::{Context, Result, bail};

use crate::config::ProxyConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Serve,
    JailSelftest,
}

/// Parse the complete production argument grammar.
///
/// The proxy is normally an implementation detail launched by `coop`, so it
/// deliberately has no extensible argument surface: unknown input must fail
/// before stdin (which carries the real credential) is read.
fn parse_mode(args: impl IntoIterator<Item = OsString>) -> Result<Mode> {
    let args: Vec<OsString> = args.into_iter().collect();
    match args.as_slice() {
        [] => Ok(Mode::Serve),
        [arg] if arg == OsStr::new("--jail-selftest") => Ok(Mode::JailSelftest),
        _ => bail!(
            "unsupported coop-proxy arguments; expected no arguments or exactly --jail-selftest"
        ),
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let mode = parse_mode(std::env::args_os().skip(1))?;

    // Diagnostic used by the integration suite to assert the jail actually
    // restricts on the host (see `jail::selftest`). Handles no config or
    // credential, so it runs before reading stdin.
    if mode == Mode::JailSelftest {
        return jail::selftest();
    }

    let cfg = read_config().context("failed to read startup config from stdin")?;

    // Self-confine before building the runtime: Landlock's domain is inherited
    // by threads created afterwards, and the multi-thread runtime spawns its
    // workers below — so applying it here covers every worker.
    //
    // Fail closed: every serving Linux process applies the jail. Tests that
    // exercise HTTP policy without a compatible Landlock host call the server
    // in-process under `cfg(test)`; the production binary has no opt-out.
    // macOS confines externally via `sandbox-exec` (coop's `proxy.rs`), so
    // this is Linux-only.
    #[cfg(target_os = "linux")]
    let net_scoped =
        jail::apply(cfg.listen.port()).context("failed to establish the Landlock jail")?;
    #[cfg(target_os = "linux")]
    if net_scoped {
        tracing::info!(
            "Landlock jail established: filesystem writes and program exec denied; \
             TCP egress limited to :443/:53"
        );
    } else {
        tracing::warn!(
            "Landlock jail established with reduced scope: filesystem writes and program \
             exec denied, but this kernel (<6.7) has no Landlock network rules, so TCP \
             egress is not port-scoped (open egress). Upstream identity is still enforced \
             at the TLS layer."
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async move { proxy::serve(cfg, shutdown_signal()).await })
}

/// Read the JSON startup blob from stdin to EOF, then let stdin close. The
/// secret lands in process memory only — never argv, never a file.
fn read_config() -> Result<ProxyConfig> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading stdin")?;
    ProxyConfig::from_json(&buf)
}

/// Resolve on SIGTERM (coop's teardown signal) or Ctrl-C, so the proxy stops
/// accepting new connections and exits cleanly.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => tracing::info!("received SIGTERM"),
            r = tokio::signal::ctrl_c() => {
                if let Err(e) = r {
                    tracing::warn!("ctrl_c handler error: {e}");
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("ctrl_c handler error: {e}");
        }
    }
}

#[cfg(test)]
mod gate_tests;

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn production_argument_grammar_is_closed() {
        assert_eq!(parse_mode(args(&[])).unwrap(), Mode::Serve);
        assert_eq!(
            parse_mode(args(&["--jail-selftest"])).unwrap(),
            Mode::JailSelftest
        );

        for invalid in [
            vec!["--no-jail"],
            vec!["--unknown"],
            vec!["positional"],
            vec!["--jail-selftest=true"],
            vec!["--jail-selftest", "--jail-selftest"],
            vec!["--jail-selftest", "ignored"],
        ] {
            assert!(
                parse_mode(args(&invalid)).is_err(),
                "unexpectedly accepted {invalid:?}"
            );
        }
    }
}
