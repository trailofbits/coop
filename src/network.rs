use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::cmd::Cmd;
use crate::config::{AbsoluteHostToolPath, HostInterface, Instance, NetworkConfig};
use crate::host_tool::{
    ResolvedHostTool, TrustedLaunchContext, TrustedToolPolicy, resolve_exact_host_tool,
    resolve_host_tool,
};

const BRIDGE_NAME: &str = "br0";

/// Match spec for the rule that drops routed guest-to-guest traffic. Shared by
/// the `-C` probe, the `-I` insert, and the `-D` teardown so the three cannot
/// drift — a teardown that misses by one argument leaks the rule.
const GUEST_ISOLATION_SPEC: [&str; 6] = ["-i", BRIDGE_NAME, "-o", BRIDGE_NAME, "-j", "DROP"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NetworkTool {
    Sudo,
    Ip,
    Bridge,
    Iptables,
    Sysctl,
}

impl NetworkTool {
    fn name(self) -> &'static str {
        match self {
            Self::Sudo => "sudo",
            Self::Ip => "ip",
            Self::Bridge => "bridge",
            Self::Iptables => "iptables",
            Self::Sysctl => "sysctl",
        }
    }

    fn production_candidates(self) -> &'static [&'static str] {
        match self {
            Self::Sudo => &["/usr/bin/sudo", "/bin/sudo"],
            Self::Ip => &["/usr/sbin/ip", "/sbin/ip", "/usr/bin/ip", "/bin/ip"],
            Self::Bridge => &[
                "/usr/sbin/bridge",
                "/sbin/bridge",
                "/usr/bin/bridge",
                "/bin/bridge",
            ],
            Self::Iptables => &[
                "/usr/sbin/iptables",
                "/sbin/iptables",
                "/usr/bin/iptables",
                "/bin/iptables",
            ],
            Self::Sysctl => &[
                "/usr/sbin/sysctl",
                "/sbin/sysctl",
                "/usr/bin/sysctl",
                "/bin/sysctl",
            ],
        }
    }

    fn configured_path(self, cfg: &NetworkConfig) -> Option<&AbsoluteHostToolPath> {
        match self {
            Self::Sudo => cfg.host_tools.sudo.as_ref(),
            Self::Ip => cfg.host_tools.ip.as_ref(),
            Self::Bridge => cfg.host_tools.bridge.as_ref(),
            Self::Iptables => cfg.host_tools.iptables.as_ref(),
            Self::Sysctl => cfg.host_tools.sysctl.as_ref(),
        }
    }
}

impl fmt::Display for NetworkTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg_attr(
    target_os = "linux",
    expect(
        clippy::unnecessary_wraps,
        reason = "the shared Firecracker module must reject accidental use on non-Linux hosts"
    )
)]
fn production_network_policy() -> Result<TrustedToolPolicy> {
    #[cfg(target_os = "linux")]
    {
        Ok(TrustedToolPolicy::new(
            "/",
            ["/usr/bin", "/usr/sbin", "/bin", "/sbin"].map(PathBuf::from),
            0,
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        bail!("Linux network control-plane tools are unavailable on this platform")
    }
}

#[cfg(all(test, target_os = "linux"))]
fn resolve_network_tool(
    tool: NetworkTool,
    cfg: &NetworkConfig,
    policy: &TrustedToolPolicy,
) -> Result<ResolvedHostTool<NetworkTool>> {
    if let Some(path) = tool.configured_path(cfg) {
        return resolve_exact_host_tool(tool, path.as_path(), policy).with_context(|| {
            format!(
                "configured network.host_tools.{} path '{}' is not trusted; built-in fallback was not attempted",
                tool.name(),
                path.as_path().display()
            )
        });
    }
    let candidates = tool
        .production_candidates()
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    resolve_network_tool_from_candidates(tool, &candidates, policy)
}

fn resolve_network_tool_from_candidates(
    tool: NetworkTool,
    candidates: &[PathBuf],
    policy: &TrustedToolPolicy,
) -> Result<ResolvedHostTool<NetworkTool>> {
    resolve_host_tool(tool, candidates, policy).map_err(Into::into)
}

struct SetupNetworkTools {
    sudo: ResolvedHostTool<NetworkTool>,
    ip: ResolvedHostTool<NetworkTool>,
    bridge: ResolvedHostTool<NetworkTool>,
    iptables: ResolvedHostTool<NetworkTool>,
    sysctl: ResolvedHostTool<NetworkTool>,
    launch: TrustedLaunchContext,
}

impl SetupNetworkTools {
    fn resolve(cfg: &NetworkConfig, policy: &TrustedToolPolicy) -> Result<Self> {
        Self::resolve_with(cfg, policy, |tool| {
            tool.production_candidates()
                .iter()
                .map(PathBuf::from)
                .collect()
        })
    }

    fn resolve_with(
        cfg: &NetworkConfig,
        policy: &TrustedToolPolicy,
        candidates: impl Fn(NetworkTool) -> Vec<PathBuf>,
    ) -> Result<Self> {
        Ok(Self {
            sudo: resolve_network_tool_with_candidates(
                NetworkTool::Sudo,
                cfg,
                &candidates(NetworkTool::Sudo),
                policy,
            )?,
            ip: resolve_network_tool_with_candidates(
                NetworkTool::Ip,
                cfg,
                &candidates(NetworkTool::Ip),
                policy,
            )?,
            bridge: resolve_network_tool_with_candidates(
                NetworkTool::Bridge,
                cfg,
                &candidates(NetworkTool::Bridge),
                policy,
            )?,
            iptables: resolve_network_tool_with_candidates(
                NetworkTool::Iptables,
                cfg,
                &candidates(NetworkTool::Iptables),
                policy,
            )?,
            sysctl: resolve_network_tool_with_candidates(
                NetworkTool::Sysctl,
                cfg,
                &candidates(NetworkTool::Sysctl),
                policy,
            )?,
            launch: TrustedLaunchContext::system(),
        })
    }

    fn ip(&self) -> Cmd {
        self.launch.elevated(&self.sudo, &self.ip)
    }

    fn ip_probe(&self) -> Cmd {
        self.launch.command(&self.ip)
    }

    fn iptables(&self) -> Cmd {
        self.launch.elevated(&self.sudo, &self.iptables)
    }

    fn sysctl(&self) -> Cmd {
        self.launch.elevated(&self.sudo, &self.sysctl)
    }
}

trait BridgeToolset {
    fn bridge(&self) -> Cmd;
}

impl BridgeToolset for SetupNetworkTools {
    fn bridge(&self) -> Cmd {
        self.launch.elevated(&self.sudo, &self.bridge)
    }
}

#[cfg(all(test, target_os = "linux"))]
struct IsolationNetworkTools {
    sudo: ResolvedHostTool<NetworkTool>,
    bridge: ResolvedHostTool<NetworkTool>,
    launch: TrustedLaunchContext,
}

#[cfg(all(test, target_os = "linux"))]
impl IsolationNetworkTools {
    fn resolve_production() -> Result<Self> {
        let policy = production_network_policy()?;
        Ok(Self {
            sudo: resolve_network_tool(NetworkTool::Sudo, &NetworkConfig::default(), &policy)?,
            bridge: resolve_network_tool(NetworkTool::Bridge, &NetworkConfig::default(), &policy)?,
            launch: TrustedLaunchContext::system(),
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
impl BridgeToolset for IsolationNetworkTools {
    fn bridge(&self) -> Cmd {
        self.launch.elevated(&self.sudo, &self.bridge)
    }
}

struct CleanupNetworkTools {
    sudo: ResolvedHostTool<NetworkTool>,
    launch: TrustedLaunchContext,
}

impl CleanupNetworkTools {
    fn resolve_with(
        cfg: &NetworkConfig,
        policy: &TrustedToolPolicy,
        candidates: impl Fn(NetworkTool) -> Vec<PathBuf>,
    ) -> Result<Self> {
        Ok(Self {
            sudo: resolve_network_tool_with_candidates(
                NetworkTool::Sudo,
                cfg,
                &candidates(NetworkTool::Sudo),
                policy,
            )?,
            launch: TrustedLaunchContext::system(),
        })
    }

    fn ip(&self, ip: &ResolvedHostTool<NetworkTool>) -> Cmd {
        self.launch.elevated(&self.sudo, ip)
    }

    fn ip_probe(&self, ip: &ResolvedHostTool<NetworkTool>) -> Cmd {
        self.launch.command(ip)
    }

    fn iptables(&self, iptables: &ResolvedHostTool<NetworkTool>) -> Cmd {
        self.launch.elevated(&self.sudo, iptables)
    }
}

fn resolve_network_tool_with_candidates(
    tool: NetworkTool,
    cfg: &NetworkConfig,
    candidates: &[PathBuf],
    policy: &TrustedToolPolicy,
) -> Result<ResolvedHostTool<NetworkTool>> {
    if let Some(path) = tool.configured_path(cfg) {
        return resolve_exact_host_tool(tool, path.as_path(), policy).with_context(|| {
            format!(
                "configured network.host_tools.{} path '{}' is not trusted; built-in fallback was not attempted",
                tool.name(),
                path.as_path().display()
            )
        });
    }
    resolve_network_tool_from_candidates(tool, candidates, policy)
}

/// Rewrite a host-visible endpoint URL into one reachable from inside the
/// guest.
///
/// A local model server runs on the host; the guest reaches it through a
/// backend-specific gateway address (`guest_host`): the TAP gateway on
/// Firecracker, `host.lima.internal` on Lima. A `localhost`/loopback host
/// in `url` is replaced with `guest_host`; any other host (a LAN IP or
/// DNS name) is passed through verbatim so a non-loopback endpoint keeps
/// working as-is.
///
/// Only the host is changed — scheme, port, path, query, and userinfo are
/// preserved.
pub fn rewrite_host_url(url: &url::Url, guest_host: &str) -> Result<url::Url> {
    if !host_is_loopback(url.host().as_ref()) {
        return Ok(url.clone());
    }
    let mut rewritten = url.clone();
    rewritten
        .set_host(Some(guest_host))
        .with_context(|| format!("Invalid guest host address '{guest_host}'"))?;
    Ok(rewritten)
}

/// Whether a URL host refers to the local machine: the literal
/// `localhost`, or any IPv4/IPv6 loopback address. Uses url's typed host
/// so bracketed IPv6 literals classify correctly.
fn host_is_loopback(host: Option<&url::Host<&str>>) -> bool {
    match host {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Ensure the bridge exists with the host IP, then create and attach the
/// instance's tap device. Sets up NAT rules if this is the first instance.
pub fn setup_tap(cfg: &NetworkConfig, inst: &Instance) -> Result<()> {
    let policy = production_network_policy()?;
    let tools = SetupNetworkTools::resolve(cfg, &policy)?;
    setup_tap_with_tools(cfg, inst, &tools)
}

#[cfg(test)]
fn setup_tap_with_policy(
    cfg: &NetworkConfig,
    inst: &Instance,
    policy: &TrustedToolPolicy,
    candidates: impl Fn(NetworkTool) -> Vec<PathBuf>,
) -> Result<()> {
    let tools = SetupNetworkTools::resolve_with(cfg, policy, candidates)?;
    setup_tap_with_tools(cfg, inst, &tools)
}

fn setup_tap_with_tools(
    cfg: &NetworkConfig,
    inst: &Instance,
    tools: &SetupNetworkTools,
) -> Result<()> {
    let host_iface = resolve_host_iface(&cfg.host_iface, tools.ip_probe())?;
    let tap = inst.tap_device();

    ensure_bridge(cfg, &host_iface, tools)?;
    // Outside ensure_bridge, which returns early on a pre-existing bridge —
    // one left by a crashed teardown must still get the rule.
    ensure_guest_isolation_rule(tools)?;

    tracing::info!("Setting up TAP device {tap} on bridge {BRIDGE_NAME}");

    // Remove existing TAP device if present (leftover from previous run)
    if tap_exists(&tap, tools.ip_probe()) {
        tracing::debug!("TAP device {tap} already exists, removing");
        if let Err(e) = tools.ip().args(["link", "del", &tap]).run() {
            tracing::debug!("Failed to remove stale TAP {tap} (non-fatal): {e}");
        }
    }

    tools
        .ip()
        .args(["tuntap", "add", &tap, "mode", "tap"])
        .run()
        .context("Failed to create TAP device")?;
    tools
        .ip()
        .args(["link", "set", &tap, "master", BRIDGE_NAME])
        .run()
        .context("Failed to add TAP to bridge")?;
    // The L2 half: the bridge never forwards between two isolated ports. Set
    // before the TAP goes up, so the port is never live and unisolated. The
    // routed path is closed separately, in ensure_guest_isolation_rule.
    isolate_tap_port(&tap, tools)?;
    tools
        .ip()
        .args(["link", "set", &tap, "up"])
        .run()
        .context("Failed to bring up TAP device")?;

    let guest_ip = inst.guest_ip();
    tracing::info!("Network configured: bridge={BRIDGE_NAME}, tap={tap}, guest={guest_ip}");
    Ok(())
}

/// Remove the instance's tap device. Tears down the bridge if no taps remain.
pub fn teardown_tap(cfg: &NetworkConfig, inst: &Instance) -> Result<()> {
    let policy = production_network_policy()?;
    teardown_tap_with_policy(cfg, inst, &policy, |tool| {
        tool.production_candidates()
            .iter()
            .map(PathBuf::from)
            .collect()
    })
}

fn teardown_tap_with_policy(
    cfg: &NetworkConfig,
    inst: &Instance,
    policy: &TrustedToolPolicy,
    candidates: impl Fn(NetworkTool) -> Vec<PathBuf>,
) -> Result<()> {
    let ip = resolve_network_tool_with_candidates(
        NetworkTool::Ip,
        cfg,
        &candidates(NetworkTool::Ip),
        policy,
    )?;
    let launch = TrustedLaunchContext::system();
    let mut tools = None;
    let tap = inst.tap_device();
    tracing::info!("Tearing down TAP device {tap}");

    if tap_exists(&tap, launch.command(&ip)) {
        let resolved = CleanupNetworkTools::resolve_with(cfg, policy, &candidates)?;
        resolved
            .ip(&ip)
            .args(["link", "del", &tap])
            .run()
            .context("Failed to delete TAP device")?;
        tools = Some(resolved);
    }

    // If no tap devices remain on the bridge, tear it down
    if bridge_exists(launch.command(&ip)) && bridge_is_empty(launch.command(&ip)) {
        let host_iface = resolve_host_iface(&cfg.host_iface, launch.command(&ip))
            .unwrap_or_else(|_| "eth0".into());
        let tools = match tools {
            Some(tools) => tools,
            None => CleanupNetworkTools::resolve_with(cfg, policy, &candidates)?,
        };
        let iptables = resolve_network_tool_with_candidates(
            NetworkTool::Iptables,
            cfg,
            &candidates(NetworkTool::Iptables),
            policy,
        );
        if let Err(error) = &iptables {
            tracing::debug!("Cannot resolve trusted iptables for cleanup (non-fatal): {error}");
        }
        teardown_bridge(&host_iface, &tools, Some(&ip), iptables.as_ref().ok());
    }

    tracing::info!("Network teardown complete");
    Ok(())
}

/// Tear down the bridge and all NAT rules unconditionally.
pub fn teardown_all(cfg: &NetworkConfig) {
    let policy = match production_network_policy() {
        Ok(policy) => policy,
        Err(error) => {
            tracing::debug!("Cannot resolve trusted network cleanup policy (non-fatal): {error}");
            return;
        }
    };
    teardown_all_with_policy(cfg, &policy, |tool| {
        tool.production_candidates()
            .iter()
            .map(PathBuf::from)
            .collect()
    });
}

fn teardown_all_with_policy(
    cfg: &NetworkConfig,
    policy: &TrustedToolPolicy,
    candidates: impl Fn(NetworkTool) -> Vec<PathBuf>,
) {
    let tools = match CleanupNetworkTools::resolve_with(cfg, policy, &candidates) {
        Ok(tools) => tools,
        Err(error) => {
            tracing::debug!("Cannot resolve trusted network cleanup tools (non-fatal): {error}");
            return;
        }
    };
    let ip = resolve_network_tool_with_candidates(
        NetworkTool::Ip,
        cfg,
        &candidates(NetworkTool::Ip),
        policy,
    );
    if let Err(error) = &ip {
        tracing::debug!("Cannot resolve trusted ip for cleanup (non-fatal): {error}");
    }
    let host_iface = match (&cfg.host_iface, ip.as_ref().ok()) {
        (HostInterface::Named(name), _) => name.as_str().to_string(),
        (HostInterface::Auto, Some(ip)) => {
            detect_default_iface(tools.ip_probe(ip)).unwrap_or_else(|_| "eth0".into())
        }
        (HostInterface::Auto, None) => "eth0".to_string(),
    };
    let iptables = resolve_network_tool_with_candidates(
        NetworkTool::Iptables,
        cfg,
        &candidates(NetworkTool::Iptables),
        policy,
    );
    if let Err(error) = &iptables {
        tracing::debug!("Cannot resolve trusted iptables for cleanup (non-fatal): {error}");
    }
    teardown_bridge(
        &host_iface,
        &tools,
        ip.as_ref().ok(),
        iptables.as_ref().ok(),
    );
}

// ── Guest-to-guest isolation ──────────────────────────────────

/// Apply the L2 half to one TAP and confirm it took effect.
fn isolate_tap_port(tap: &str, tools: &impl BridgeToolset) -> Result<()> {
    tools
        .bridge()
        .args(["link", "set", "dev", tap, "isolated", "on"])
        .run()
        .context("Failed to isolate TAP from peer guest ports")?;
    let flags = tools
        .bridge()
        .args(["-d", "link", "show", "dev", tap])
        .capture()
        .with_context(|| format!("Failed to read back bridge port flags for {tap}"))?;
    if !port_is_isolated(&flags) {
        bail!(
            "Bridge port {tap} did not accept the isolated flag, so peer guest VMs would be \
             reachable from this one. Guest-to-guest isolation needs Linux >= 4.18 and \
             iproute2 >= 4.19."
        );
    }
    Ok(())
}

/// Whether `bridge -d link show dev <tap>` output reports the port isolated.
///
/// The readback exists because the set can succeed while doing nothing:
/// `IFLA_BRPORT_ISOLATED` is attribute 33, and a kernel below 4.18 caps the
/// bridge-port policy at 32 and silently drops out-of-range attributes, so
/// `bridge` exits 0 on a port that is not isolated. `-d` is required — the
/// flag is printed only in the detailed section.
fn port_is_isolated(flags: &str) -> bool {
    flags.contains("isolated on")
}

/// The L3 half: drop guest-to-guest traffic the host would otherwise route.
///
/// Port isolation governs only port-to-port forwarding. A frame a guest
/// addresses to the bridge is local delivery, so the flag never applies, and
/// `ip_forward` then sends it back out `br0` from the bridge device — which
/// has no isolated source port either. Nothing else in the ruleset matches
/// that, so without this it falls through to the FORWARD policy, ACCEPT on a
/// stock host.
///
/// Inserted at the head so it cannot lose to a pre-existing permissive
/// `-A FORWARD -j ACCEPT` from libvirt or another tool. Existing rules are
/// checked for precedence too; refuse startup if the firewall has shadowed it.
fn ensure_guest_isolation_rule(tools: &SetupNetworkTools) -> Result<()> {
    let present = tools
        .iptables()
        .args(["-C", "FORWARD"])
        .args(GUEST_ISOLATION_SPEC)
        .capture()
        .is_ok();
    if !present {
        tools
            .iptables()
            .args(["-I", "FORWARD", "1"])
            .args(GUEST_ISOLATION_SPEC)
            .run()
            .context("Failed to deny inter-guest routing across the bridge")?;
    }
    let rules = tools
        .iptables()
        .args(["-S", "FORWARD"])
        .capture()
        .context("Failed to verify guest isolation rule precedence")?;
    if !guest_isolation_rule_is_first(&rules) {
        bail!(
            "Guest isolation DROP must be the first FORWARD rule; \
             move it ahead of other rules in the host firewall configuration before starting a VM"
        );
    }
    Ok(())
}

/// Ignore the chain policy; the first rule must enforce guest isolation.
fn guest_isolation_rule_is_first(rules: &str) -> bool {
    rules
        .lines()
        .find(|line| line.starts_with("-A "))
        .is_some_and(|line| {
            line.split_whitespace()
                .eq(["-A", "FORWARD"].into_iter().chain(GUEST_ISOLATION_SPEC))
        })
}

// ── Bridge management ─────────────────────────────────────────

fn ensure_bridge(cfg: &NetworkConfig, host_iface: &str, tools: &SetupNetworkTools) -> Result<()> {
    if bridge_exists(tools.ip_probe()) {
        tracing::debug!("Bridge {BRIDGE_NAME} already exists");
        return Ok(());
    }

    tracing::info!("Creating bridge {BRIDGE_NAME}");
    tools
        .ip()
        .args(["link", "add", BRIDGE_NAME, "type", "bridge"])
        .run()
        .context("Failed to create bridge")?;

    let host_cidr = format!("{}{}", cfg.host_ip, cfg.subnet_mask);
    tools
        .ip()
        .args(["addr", "add", &host_cidr, "dev", BRIDGE_NAME])
        .run()
        .context("Failed to assign IP to bridge")?;

    tools
        .ip()
        .args(["link", "set", BRIDGE_NAME, "up"])
        .run()
        .context("Failed to bring up bridge")?;

    tools
        .sysctl()
        .args(["-w", "net.ipv4.ip_forward=1"])
        .run()
        .context("Failed to enable IP forwarding")?;

    tools
        .iptables()
        .args([
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-o",
            host_iface,
            "-j",
            "MASQUERADE",
        ])
        .run()
        .context("Failed to add NAT masquerade rule")?;

    tools
        .iptables()
        .args([
            "-A",
            "FORWARD",
            "-i",
            BRIDGE_NAME,
            "-o",
            host_iface,
            "-j",
            "ACCEPT",
        ])
        .run()
        .context("Failed to add forward rule")?;

    tools
        .iptables()
        .args([
            "-A",
            "FORWARD",
            "-i",
            host_iface,
            "-o",
            BRIDGE_NAME,
            "-m",
            "state",
            "--state",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ])
        .run()
        .context("Failed to add return traffic rule")?;

    Ok(())
}

fn teardown_bridge(
    host_iface: &str,
    tools: &CleanupNetworkTools,
    ip: Option<&ResolvedHostTool<NetworkTool>>,
    iptables: Option<&ResolvedHostTool<NetworkTool>>,
) {
    tracing::info!("Tearing down bridge {BRIDGE_NAME}");

    let Some(iptables) = iptables else {
        teardown_bridge_device(tools, ip);
        return;
    };

    if let Err(e) = tools
        .iptables(iptables)
        .args(["-D", "FORWARD"])
        .args(GUEST_ISOLATION_SPEC)
        .run()
    {
        tracing::debug!("Failed to remove guest isolation rule (non-fatal): {e}");
    }
    if let Err(e) = tools
        .iptables(iptables)
        .args([
            "-t",
            "nat",
            "-D",
            "POSTROUTING",
            "-o",
            host_iface,
            "-j",
            "MASQUERADE",
        ])
        .run()
    {
        tracing::debug!("Failed to remove NAT rule (non-fatal): {e}");
    }
    if let Err(e) = tools
        .iptables(iptables)
        .args([
            "-D",
            "FORWARD",
            "-i",
            BRIDGE_NAME,
            "-o",
            host_iface,
            "-j",
            "ACCEPT",
        ])
        .run()
    {
        tracing::debug!("Failed to remove forward rule (non-fatal): {e}");
    }
    if let Err(e) = tools
        .iptables(iptables)
        .args([
            "-D",
            "FORWARD",
            "-i",
            host_iface,
            "-o",
            BRIDGE_NAME,
            "-m",
            "state",
            "--state",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ])
        .run()
    {
        tracing::debug!("Failed to remove return traffic rule (non-fatal): {e}");
    }

    teardown_bridge_device(tools, ip);
}

fn teardown_bridge_device(tools: &CleanupNetworkTools, ip: Option<&ResolvedHostTool<NetworkTool>>) {
    let Some(ip) = ip else {
        return;
    };
    if bridge_exists(tools.ip_probe(ip)) {
        if let Err(e) = tools
            .ip(ip)
            .args(["link", "set", BRIDGE_NAME, "down"])
            .run()
        {
            tracing::debug!("Failed to bring down bridge (non-fatal): {e}");
        }
        if let Err(e) = tools.ip(ip).args(["link", "del", BRIDGE_NAME]).run() {
            tracing::debug!("Failed to delete bridge (non-fatal): {e}");
        }
    }
}

fn bridge_exists(ip: Cmd) -> bool {
    ip.args(["link", "show", BRIDGE_NAME]).status_ok()
}

fn bridge_is_empty(ip: Cmd) -> bool {
    let output = ip.args(["link", "show", "master", BRIDGE_NAME]).output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => true,
    }
}

// ── Helpers ───────────────────────────────────────────────────

fn resolve_host_iface(configured: &HostInterface, ip: Cmd) -> Result<String> {
    match configured {
        HostInterface::Auto => detect_default_iface(ip),
        HostInterface::Named(name) => Ok(name.as_str().to_string()),
    }
}

fn detect_default_iface(ip: Cmd) -> Result<String> {
    let output = ip
        .args(["route", "show", "default"])
        .output()
        .context("Failed to detect default network interface")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Format: "default via X.X.X.X dev IFACE ..."
    let iface = stdout
        .split_whitespace()
        .skip_while(|w| *w != "dev")
        .nth(1)
        .context("Could not parse default route — set network.host_iface in config")?
        .to_string();
    tracing::debug!("Auto-detected host interface: {iface}");
    Ok(iface)
}

fn tap_exists(name: &str, ip: Cmd) -> bool {
    ip.args(["link", "show", name]).status_ok()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::path::Path;

    use super::*;
    use crate::config::{InstanceIndex, InstanceName, default_image_name};

    fn rewrite(url: &str, host: &str) -> String {
        rewrite_host_url(&url::Url::parse(url).unwrap(), host)
            .unwrap()
            .to_string()
    }

    #[test]
    fn rewrites_localhost_to_guest_host() {
        assert_eq!(
            rewrite("http://localhost:11434", "172.16.0.1"),
            "http://172.16.0.1:11434/"
        );
    }

    #[test]
    fn rewrites_loopback_ipv4_to_guest_host() {
        assert_eq!(
            rewrite("http://127.0.0.1:11434/v1/", "host.lima.internal"),
            "http://host.lima.internal:11434/v1/"
        );
    }

    #[test]
    fn rewrites_loopback_ipv6_to_guest_host() {
        assert_eq!(
            rewrite("http://[::1]:8080/", "172.16.0.1"),
            "http://172.16.0.1:8080/"
        );
    }

    #[test]
    fn passes_lan_host_through_unchanged() {
        // A non-loopback endpoint already reaches the host network; leave it.
        assert_eq!(
            rewrite("http://192.168.1.50:11434/", "172.16.0.1"),
            "http://192.168.1.50:11434/"
        );
        assert_eq!(
            rewrite("http://models.lan:11434/", "172.16.0.1"),
            "http://models.lan:11434/"
        );
    }

    #[test]
    fn preserves_scheme_port_and_path() {
        assert_eq!(
            rewrite("https://localhost:9999/v1/chat?a=b", "10.0.0.1"),
            "https://10.0.0.1:9999/v1/chat?a=b"
        );
    }

    #[test]
    fn guest_isolation_requires_the_first_forward_rule() {
        let drop = "-A FORWARD -i br0 -o br0 -j DROP";
        let accept = "-A FORWARD -j ACCEPT";
        assert!(guest_isolation_rule_is_first(&format!(
            "-P FORWARD ACCEPT\n{drop}\n{accept}\n"
        )));
        assert!(!guest_isolation_rule_is_first(&format!(
            "-P FORWARD ACCEPT\n{accept}\n{drop}\n"
        )));
        assert!(!guest_isolation_rule_is_first("-P FORWARD DROP\n"));
        assert!(!guest_isolation_rule_is_first(""));
        assert!(!guest_isolation_rule_is_first(
            "-A FORWARD -i br0 -o eth0 -j DROP"
        ));
    }

    #[test]
    fn port_is_isolated_reads_the_detailed_flag() {
        // Real `bridge -d link show dev tap0` shapes: the flag is printed as
        // `isolated on` / `isolated off`, and is absent entirely on a kernel
        // that does not know the attribute — the silent-no-op case.
        let isolated = "6: tap0: <BROADCAST,MULTICAST> mtu 1500 master br0 state disabled \
             priority 32 cost 100 \n    hairpin off guard off root_block off fastleave off \
             learning on flood on mcast_flood on neigh_suppress off vlan_tunnel off isolated on ";
        let not_isolated = isolated.replace("isolated on", "isolated off");
        let no_attribute = "6: tap0: <BROADCAST,MULTICAST> mtu 1500 master br0 state disabled \
             priority 32 cost 100 \n    hairpin off guard off root_block off fastleave off \
             learning on flood on mcast_flood on neigh_suppress off vlan_tunnel off ";

        assert!(port_is_isolated(isolated));
        assert!(!port_is_isolated(&not_isolated));
        assert!(!port_is_isolated(no_attribute));
        assert!(!port_is_isolated(""));
    }

    #[test]
    fn host_is_loopback_classifies_correctly() {
        let loopback = |h: &str| {
            host_is_loopback(
                url::Url::parse(&format!("http://{h}"))
                    .unwrap()
                    .host()
                    .as_ref(),
            )
        };
        assert!(loopback("localhost"));
        assert!(loopback("LocalHost"));
        assert!(loopback("127.0.0.1"));
        assert!(loopback("127.5.5.5"));
        assert!(loopback("[::1]"));
        assert!(!loopback("192.168.0.1"));
        assert!(!loopback("example.com"));
        assert!(!host_is_loopback(None));
    }

    #[test]
    fn network_tool_identities_have_only_fixed_absolute_candidates() {
        assert_eq!(
            NetworkTool::Sudo.production_candidates(),
            ["/usr/bin/sudo", "/bin/sudo"]
        );
        for (tool, name) in [
            (NetworkTool::Ip, "ip"),
            (NetworkTool::Bridge, "bridge"),
            (NetworkTool::Iptables, "iptables"),
            (NetworkTool::Sysctl, "sysctl"),
        ] {
            assert_eq!(
                tool.production_candidates(),
                [
                    format!("/usr/sbin/{name}"),
                    format!("/sbin/{name}"),
                    format!("/usr/bin/{name}"),
                    format!("/bin/{name}"),
                ]
            );
            assert!(
                tool.production_candidates()
                    .iter()
                    .all(|candidate| Path::new(candidate).is_absolute())
            );
        }
    }

    #[test]
    fn setup_resolves_every_tool_before_running_the_first_probe() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().join("root");
        let bin = anchor.join("usr/bin");
        let markers = temp.path().join("markers");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir(&markers).unwrap();
        for name in ["sudo", "ip", "bridge", "iptables"] {
            let executable = bin.join(name);
            fs::write(
                &executable,
                format!(
                    "#!/bin/sh\n: > '{}'\nexit 0\n",
                    markers.join(name).display()
                ),
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let policy =
            TrustedToolPolicy::new(&anchor, [bin.clone()], fs::metadata(&anchor).unwrap().uid());
        let cfg = NetworkConfig::default();
        let inst = Instance {
            name: InstanceName::new("resolution-order").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: temp.path().join("instance"),
            image: default_image_name(),
        };
        let error = setup_tap_with_policy(&cfg, &inst, &policy, |tool| vec![bin.join(tool.name())])
            .unwrap_err();
        assert!(error.to_string().contains("trusted host tool 'sysctl'"));
        assert_eq!(fs::read_dir(markers).unwrap().count(), 0);
    }

    #[test]
    fn configured_exact_path_replaces_builtin_candidates_without_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().join("root");
        let bin = anchor.join("usr/bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["sudo", "ip", "bridge", "iptables", "sysctl"] {
            let executable = bin.join(name);
            fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let configured_missing = anchor.join("run/current-system/sw/bin/ip");
        let cfg: NetworkConfig = toml::from_str(&format!(
            "[host_tools]\nip = {:?}\n",
            configured_missing.display().to_string()
        ))
        .unwrap();
        let policy =
            TrustedToolPolicy::new(&anchor, [bin.clone()], fs::metadata(&anchor).unwrap().uid());

        let result =
            SetupNetworkTools::resolve_with(&cfg, &policy, |tool| vec![bin.join(tool.name())]);
        assert!(
            result.is_err(),
            "configured missing path must not fall back"
        );
        let error = result.err().unwrap();
        let message = error.to_string();
        assert!(message.contains("network.host_tools.ip"));
        assert!(message.contains("built-in fallback was not attempted"));
        assert!(message.contains(&configured_missing.display().to_string()));
    }

    #[test]
    fn configured_paths_can_select_trusted_non_fhs_targets() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().join("root");
        let bin = anchor.join("usr/bin");
        let profile = anchor.join("run/current-system/sw/bin");
        let wrappers = anchor.join("run/wrappers/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir_all(&wrappers).unwrap();
        for name in ["ip", "bridge", "iptables", "sysctl"] {
            let executable = profile.join(name);
            fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let sudo = wrappers.join("sudo");
        fs::write(&sudo, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();
        let cfg: NetworkConfig = toml::from_str(&format!(
            "[host_tools]\nsudo = {sudo:?}\nip = {ip:?}\nbridge = {bridge:?}\niptables = {iptables:?}\nsysctl = {sysctl:?}\n",
            sudo = sudo.display().to_string(),
            ip = profile.join("ip").display().to_string(),
            bridge = profile.join("bridge").display().to_string(),
            iptables = profile.join("iptables").display().to_string(),
            sysctl = profile.join("sysctl").display().to_string(),
        ))
        .unwrap();
        let policy =
            TrustedToolPolicy::new(&anchor, [bin.clone()], fs::metadata(&anchor).unwrap().uid());
        let tools =
            SetupNetworkTools::resolve_with(&cfg, &policy, |tool| vec![bin.join(tool.name())])
                .unwrap();

        assert_eq!(tools.sudo.launch_path(), sudo);
        assert_eq!(tools.ip.launch_path(), profile.join("ip"));
        assert_eq!(tools.bridge.launch_path(), profile.join("bridge"));
        assert_eq!(tools.iptables.launch_path(), profile.join("iptables"));
        assert_eq!(tools.sysctl.launch_path(), profile.join("sysctl"));
    }

    #[test]
    fn teardown_tap_uses_configured_exact_ip_and_sudo_paths() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().join("root");
        let profile = anchor.join("run/current-system/sw/bin");
        let wrappers = anchor.join("run/wrappers/bin");
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir_all(&wrappers).unwrap();
        let marker = temp.path().join("runs");
        let inst = Instance {
            name: InstanceName::new("exact-cleanup").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: temp.path().join("instance"),
            image: default_image_name(),
        };
        let tap = inst.tap_device();

        let ip = profile.join("ip");
        fs::write(
            &ip,
            format!(
                "#!/bin/sh\nprintf 'ip:%s\\n' \"$*\" >> '{}'\n\
                 if [ \"$*\" = 'link show {tap}' ]; then exit 0; fi\n\
                 if [ \"$*\" = 'link del {tap}' ]; then exit 0; fi\n\
                 exit 1\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ip, fs::Permissions::from_mode(0o755)).unwrap();

        let sudo = wrappers.join("sudo");
        fs::write(
            &sudo,
            format!(
                "#!/bin/sh\nprintf 'sudo\\n' >> '{}'\n[ \"$1\" = '--' ] || exit 97\nshift\nexec \"$@\"\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();

        let cfg: NetworkConfig = toml::from_str(&format!(
            "[host_tools]\nsudo = {sudo:?}\nip = {ip:?}\n",
            sudo = sudo.display().to_string(),
            ip = ip.display().to_string(),
        ))
        .unwrap();
        let policy = TrustedToolPolicy::new(
            &anchor,
            std::iter::empty::<PathBuf>(),
            fs::metadata(&anchor).unwrap().uid(),
        );

        teardown_tap_with_policy(&cfg, &inst, &policy, |_| Vec::new()).unwrap();

        let runs = fs::read_to_string(marker).unwrap();
        assert!(runs.contains(&format!("ip:link show {tap}\n")));
        assert!(runs.contains("sudo\n"));
        assert!(runs.contains(&format!("ip:link del {tap}\n")));
    }

    #[test]
    fn teardown_tap_does_not_require_sudo_when_nothing_needs_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().join("root");
        let profile = anchor.join("run/current-system/sw/bin");
        fs::create_dir_all(&profile).unwrap();
        let marker = temp.path().join("ip-probes");
        let ip = profile.join("ip");
        fs::write(
            &ip,
            format!(
                "#!/bin/sh\nprintf 'probe\\n' >> '{}'\nexit 1\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ip, fs::Permissions::from_mode(0o755)).unwrap();
        let missing_sudo = anchor.join("run/wrappers/bin/sudo");
        let cfg: NetworkConfig = toml::from_str(&format!(
            "[host_tools]\nsudo = {sudo:?}\nip = {ip:?}\n",
            sudo = missing_sudo.display().to_string(),
            ip = ip.display().to_string(),
        ))
        .unwrap();
        let policy = TrustedToolPolicy::new(
            &anchor,
            std::iter::empty::<PathBuf>(),
            fs::metadata(&anchor).unwrap().uid(),
        );
        let inst = Instance {
            name: InstanceName::new("idempotent-cleanup").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: temp.path().join("instance"),
            image: default_image_name(),
        };

        teardown_tap_with_policy(&cfg, &inst, &policy, |_| Vec::new()).unwrap();

        assert_eq!(fs::read_to_string(marker).unwrap(), "probe\nprobe\n");
    }

    #[test]
    fn teardown_all_keeps_firewall_cleanup_when_ip_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().join("root");
        let bin = anchor.join("usr/bin");
        fs::create_dir_all(&bin).unwrap();
        let marker = temp.path().join("iptables-runs");

        let sudo = bin.join("sudo");
        fs::write(&sudo, "#!/bin/sh\nshift\nexec \"$@\"\n").unwrap();
        fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();
        let iptables = bin.join("iptables");
        fs::write(
            &iptables,
            format!("#!/bin/sh\nprintf 'run\\n' >> '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&iptables, fs::Permissions::from_mode(0o755)).unwrap();

        let policy =
            TrustedToolPolicy::new(&anchor, [bin.clone()], fs::metadata(&anchor).unwrap().uid());
        teardown_all_with_policy(&NetworkConfig::default(), &policy, |tool| {
            vec![bin.join(tool.name())]
        });
        assert_eq!(fs::read_to_string(marker).unwrap(), "run\n".repeat(4));
    }

    #[test]
    fn teardown_exact_ip_failure_does_not_suppress_exact_iptables_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().join("root");
        let bin = anchor.join("usr/bin");
        let profile = anchor.join("run/current-system/sw/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&profile).unwrap();
        let marker = temp.path().join("iptables-runs");

        let sudo = bin.join("sudo");
        fs::write(&sudo, "#!/bin/sh\nshift\nexec \"$@\"\n").unwrap();
        fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();
        let iptables = profile.join("iptables");
        fs::write(
            &iptables,
            format!("#!/bin/sh\nprintf 'run\\n' >> '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&iptables, fs::Permissions::from_mode(0o755)).unwrap();
        let missing_ip = profile.join("ip");
        let cfg: NetworkConfig = toml::from_str(&format!(
            "[host_tools]\nip = {ip:?}\niptables = {iptables:?}\n",
            ip = missing_ip.display().to_string(),
            iptables = iptables.display().to_string(),
        ))
        .unwrap();
        let policy =
            TrustedToolPolicy::new(&anchor, [bin.clone()], fs::metadata(&anchor).unwrap().uid());

        teardown_all_with_policy(&cfg, &policy, |tool| vec![bin.join(tool.name())]);
        assert_eq!(fs::read_to_string(marker).unwrap(), "run\n".repeat(4));
    }
}

#[cfg(all(test, target_os = "linux"))]
#[expect(clippy::unwrap_used, reason = "tests")]
mod isolation_tests;
