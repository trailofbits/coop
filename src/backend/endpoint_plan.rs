//! How a guest reaches a local-model server bound to the host's loopback
//! interface: URL rewriting and the per-instance reverse tunnels it needs.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use crate::config::{CoopConfig, LocalModel};
use crate::model_state::ModelState;

/// How a guest reaches a server bound to the host's loopback interface
/// (a local model endpoint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalEndpointRoute {
    /// The guest routes to the host at this address, so loopback endpoint
    /// URLs are rewritten to it (Firecracker's TAP gateway, Lima's
    /// `host.lima.internal`).
    HostAddress(String),
    /// The guest has no route to the host; each loopback endpoint is carried
    /// over a per-instance `ssh -R` tunnel onto the guest's own loopback.
    #[cfg_attr(
        all(not(feature = "apple-container"), not(test)),
        expect(dead_code, reason = "constructed only by the apple-container backend")
    )]
    ReverseTunnel,
}

/// A reverse tunnel carrying a host-loopback server into the guest:
/// `ssh -R 127.0.0.1:<guest_port>:<host_addr>:<host_port>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReverseTunnel {
    /// Guest-loopback listener port; always unprivileged, so the non-root
    /// guest user's sshd can bind it.
    pub guest_port: u16,
    /// Host loopback address the endpoint URL named.
    pub host_addr: std::net::Ipv4Addr,
    pub host_port: u16,
}

/// Offset applied to a privileged endpoint port (< 1024) to get its
/// guest listener port: `https://localhost` (443) listens on guest 40443.
const PRIVILEGED_PORT_OFFSET: u16 = 40_000;

/// How one local-model endpoint is reached from inside the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEndpointPlan {
    /// URL written into the guest agent's configuration.
    pub guest_url: url::Url,
    /// The per-instance reverse tunnel the URL depends on, when the route
    /// needs one.
    pub tunnel: Option<ReverseTunnel>,
}

/// Resolve how the guest reaches `host_url` over `route`.
///
/// Non-loopback URLs pass through unchanged on every route. Over a reverse
/// tunnel the guest listener is on `127.0.0.1`, and the tunnel forwards to the
/// exact host loopback address and port the URL names. `localhost` and
/// `127.0.0.1` URLs keep their host (and with it any TLS server name); other
/// IPv4 loopback addresses are rewritten to `127.0.0.1` only for plain HTTP,
/// since rewriting an HTTPS host would change the name verified against the
/// certificate. A privileged port moves to an unprivileged guest port (the
/// URL's port changes; its host does not). IPv6 loopback endpoints are
/// rejected: the tunnel listens on IPv4 only.
pub fn plan_local_endpoint(
    route: &LocalEndpointRoute,
    host_url: &url::Url,
) -> Result<LocalEndpointPlan> {
    let host = match route {
        LocalEndpointRoute::HostAddress(addr) => {
            return Ok(LocalEndpointPlan {
                guest_url: crate::network::rewrite_host_url(host_url, addr)?,
                tunnel: None,
            });
        }
        LocalEndpointRoute::ReverseTunnel => host_url.host(),
    };
    let (host_addr, needs_rewrite) = match host {
        Some(url::Host::Domain(d)) if d.eq_ignore_ascii_case("localhost") => {
            (std::net::Ipv4Addr::LOCALHOST, false)
        }
        Some(url::Host::Ipv4(ip)) if ip == std::net::Ipv4Addr::LOCALHOST => (ip, false),
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => (ip, true),
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => bail!(
            "Local model endpoint {host_url} uses IPv6 loopback, which this backend \
             cannot tunnel into the guest; use http://127.0.0.1:<port> instead"
        ),
        _ => {
            return Ok(LocalEndpointPlan {
                guest_url: host_url.clone(),
                tunnel: None,
            });
        }
    };
    let host_port = host_url
        .port_or_known_default()
        .with_context(|| format!("Local model endpoint {host_url} has no port"))?;
    let guest_port = if host_port < 1024 {
        host_port + PRIVILEGED_PORT_OFFSET
    } else {
        host_port
    };
    let mut guest_url = host_url.clone();
    if needs_rewrite {
        if host_url.scheme() != "http" {
            bail!(
                "Local model endpoint {host_url} would need its host rewritten to \
                 127.0.0.1, which changes the TLS server name; use \
                 https://localhost:{host_port} or https://127.0.0.1:{host_port} instead"
            );
        }
        guest_url
            .set_host(Some("127.0.0.1"))
            .context("Failed to rewrite local model endpoint host")?;
    }
    if guest_port != host_port {
        guest_url
            .set_port(Some(guest_port))
            .map_err(|()| anyhow::anyhow!("Failed to set guest port on {host_url}"))?;
    }
    Ok(LocalEndpointPlan {
        guest_url,
        tunnel: Some(ReverseTunnel {
            guest_port,
            host_addr,
            host_port,
        }),
    })
}

/// Every reverse tunnel this instance's current model configuration needs,
/// across both agents, keyed by guest port. Two endpoints that need the same
/// guest port must also share the host destination.
pub(crate) fn local_endpoint_tunnels(
    state: &ModelState,
    cfg: &CoopConfig,
    route: &LocalEndpointRoute,
) -> Result<BTreeMap<u16, ReverseTunnel>> {
    let mut tunnels = BTreeMap::new();
    let endpoints = [
        local_endpoint(state, state.resolved_claude(&cfg.claude)),
        local_endpoint(state, state.resolved_codex(&cfg.codex)),
    ];
    for ep in endpoints.into_iter().flatten() {
        let Some(tunnel) = plan_local_endpoint(route, ep.host_url())?.tunnel else {
            continue;
        };
        match tunnels.insert(tunnel.guest_port, tunnel) {
            Some(other) if other != tunnel => bail!(
                "Local model endpoints {}:{} and {}:{} both need guest port {}; \
                 use distinct ports",
                other.host_addr,
                other.host_port,
                tunnel.host_addr,
                tunnel.host_port,
                tunnel.guest_port
            ),
            _ => {}
        }
    }
    Ok(tunnels)
}

/// Gate an already-resolved endpoint on the VM being in local mode. In
/// remote mode the materialization is intentionally empty so cloud
/// defaults apply.
pub(crate) fn local_endpoint<'a>(
    state: &ModelState,
    resolved: Option<&'a LocalModel>,
) -> Option<&'a LocalModel> {
    match state.mode {
        crate::model_state::ModelMode::Local => resolved,
        crate::model_state::ModelMode::Remote => None,
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use crate::backend::{
        LocalEndpointRoute, ReverseTunnel, local_endpoint_tunnels, plan_local_endpoint,
    };
    use crate::config::CoopConfig;
    use crate::model_state::ModelState;

    fn test_route() -> LocalEndpointRoute {
        LocalEndpointRoute::HostAddress("172.16.0.1".into())
    }

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn plan_host_address_rewrites_loopback_without_tunnel() {
        let plan = plan_local_endpoint(&test_route(), &url("http://localhost:11434/v1")).unwrap();
        assert_eq!(plan.guest_url.as_str(), "http://172.16.0.1:11434/v1");
        assert_eq!(plan.tunnel, None);
    }

    fn tunnel(guest_port: u16, host: [u8; 4], host_port: u16) -> ReverseTunnel {
        ReverseTunnel {
            guest_port,
            host_addr: host.into(),
            host_port,
        }
    }

    #[test]
    fn plan_reverse_tunnel_keeps_localhost_and_tunnels_port() {
        let route = LocalEndpointRoute::ReverseTunnel;
        for u in ["http://localhost:11434/v1", "https://127.0.0.1:8443/api"] {
            let plan = plan_local_endpoint(&route, &url(u)).unwrap();
            assert_eq!(plan.guest_url.as_str(), u);
        }
        let plan = plan_local_endpoint(&route, &url("http://localhost:11434/v1")).unwrap();
        assert_eq!(plan.tunnel, Some(tunnel(11434, [127, 0, 0, 1], 11434)));
    }

    #[test]
    fn plan_reverse_tunnel_moves_privileged_ports() {
        let route = LocalEndpointRoute::ReverseTunnel;
        let plan = plan_local_endpoint(&route, &url("https://localhost/v1")).unwrap();
        // Host (and so the TLS name) is kept; only the port moves.
        assert_eq!(plan.guest_url.as_str(), "https://localhost:40443/v1");
        assert_eq!(plan.tunnel, Some(tunnel(40443, [127, 0, 0, 1], 443)));
    }

    #[test]
    fn plan_reverse_tunnel_forwards_to_the_named_loopback_address() {
        let route = LocalEndpointRoute::ReverseTunnel;
        let plan = plan_local_endpoint(&route, &url("http://127.0.0.2:8000/x?y=1")).unwrap();
        assert_eq!(plan.guest_url.as_str(), "http://127.0.0.1:8000/x?y=1");
        assert_eq!(plan.tunnel, Some(tunnel(8000, [127, 0, 0, 2], 8000)));
        assert!(plan_local_endpoint(&route, &url("https://127.0.0.2:8000/")).is_err());
    }

    #[test]
    fn plan_reverse_tunnel_rejects_ipv6_loopback_and_passes_remote_through() {
        let route = LocalEndpointRoute::ReverseTunnel;
        assert!(plan_local_endpoint(&route, &url("http://[::1]:8000/")).is_err());
        let plan = plan_local_endpoint(&route, &url("https://models.example.com/v1")).unwrap();
        assert_eq!(plan.guest_url.as_str(), "https://models.example.com/v1");
        assert_eq!(plan.tunnel, None);
        for remote in ["http://192.168.1.5:8000/", "http://[2001:db8::1]:8000/"] {
            let plan = plan_local_endpoint(&route, &url(remote)).unwrap();
            assert_eq!(plan.guest_url.as_str(), remote);
            assert_eq!(plan.tunnel, None);
        }
    }

    #[test]
    fn plan_reverse_tunnel_keeps_the_first_unprivileged_port() {
        let route = LocalEndpointRoute::ReverseTunnel;
        let plan = plan_local_endpoint(&route, &url("http://localhost:1024/")).unwrap();
        assert_eq!(plan.guest_url.as_str(), "http://localhost:1024/");
        assert_eq!(plan.tunnel, Some(tunnel(1024, [127, 0, 0, 1], 1024)));
        let plan = plan_local_endpoint(&route, &url("http://localhost:1023/")).unwrap();
        assert_eq!(plan.tunnel, Some(tunnel(41023, [127, 0, 0, 1], 1023)));
    }

    fn local_state(claude: &str, codex: &str) -> ModelState {
        let ep = |u: &str| crate::config::LocalModel::new(url(u), "m".to_string(), None).unwrap();
        ModelState {
            mode: crate::model_state::ModelMode::Local,
            claude_endpoint: Some(ep(claude)),
            codex_endpoint: Some(ep(codex)),
            ..Default::default()
        }
    }

    #[test]
    fn endpoint_tunnels_are_shared_across_agents_and_conflicts_rejected() {
        let cfg = CoopConfig::default();
        let route = LocalEndpointRoute::ReverseTunnel;
        let shared = local_state("http://localhost:11434", "http://127.0.0.1:11434/v1/");
        let tunnels = local_endpoint_tunnels(&shared, &cfg, &route).unwrap();
        assert_eq!(tunnels.len(), 1, "one tunnel serves both agents");

        let clash = local_state("http://127.0.0.1:8000", "http://127.0.0.2:8000/v1/");
        assert!(local_endpoint_tunnels(&clash, &cfg, &route).is_err());

        let remote = ModelState::default();
        assert!(
            local_endpoint_tunnels(&remote, &cfg, &route)
                .unwrap()
                .is_empty()
        );
        assert!(
            local_endpoint_tunnels(&shared, &cfg, &test_route())
                .unwrap()
                .is_empty()
        );
    }
}
