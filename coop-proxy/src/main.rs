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

use std::io::Read;

use anyhow::{Context, Result};

use crate::config::ProxyConfig;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Diagnostic used by the integration suite to assert the jail actually
    // restricts on the host (see `jail::selftest`). Handles no config or
    // credential, so it runs before reading stdin.
    if args.iter().any(|a| a == "--jail-selftest") {
        return jail::selftest();
    }

    let cfg = read_config().context("failed to read startup config from stdin")?;

    // Self-confine before building the runtime: Landlock's domain is inherited
    // by threads created afterwards, and the multi-thread runtime spawns its
    // workers below — so applying it here covers every worker.
    //
    // Fail-closed by default: on Linux the credential proxy always confines
    // unless the explicit `--no-jail` opt-out is passed. Only the in-repo
    // `coop-proxy` tests pass it (they run on hosts that may lack Landlock);
    // `coop` never does, so there is no path where the launcher serves the
    // credential proxy unconfined. macOS confines externally via `sandbox-exec`
    // (coop's `proxy.rs`), so this is Linux-only.
    #[cfg(target_os = "linux")]
    if !args.iter().any(|a| a == "--no-jail") {
        jail::apply(cfg.listen.port()).context("failed to establish the Landlock jail")?;
        tracing::info!(
            "Landlock jail established: filesystem writes and program exec denied; \
             TCP egress limited to :443/:53"
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
