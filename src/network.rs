use std::process::Command;

use anyhow::{Context, Result};

use crate::cmd::Cmd;
use crate::config::{HostInterface, Instance, NetworkConfig};

const BRIDGE_NAME: &str = "br0";

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
    let host_iface = resolve_host_iface(&cfg.host_iface)?;
    let tap = inst.tap_device();

    ensure_bridge(cfg, &host_iface)?;

    tracing::info!("Setting up TAP device {tap} on bridge {BRIDGE_NAME}");

    // Remove existing TAP device if present (leftover from previous run)
    if tap_exists(&tap) {
        tracing::debug!("TAP device {tap} already exists, removing");
        if let Err(e) = Cmd::new("ip").args(["link", "del", &tap]).sudo().run() {
            tracing::debug!("Failed to remove stale TAP {tap} (non-fatal): {e}");
        }
    }

    Cmd::new("ip")
        .args(["tuntap", "add", &tap, "mode", "tap"])
        .sudo()
        .run()
        .context("Failed to create TAP device")?;
    Cmd::new("ip")
        .args(["link", "set", &tap, "master", BRIDGE_NAME])
        .sudo()
        .run()
        .context("Failed to add TAP to bridge")?;
    // The Linux bridge forwards guest-to-guest frames at L2. Those frames do
    // not reliably traverse the host's iptables FORWARD chain, so firewall
    // rules alone cannot establish VM isolation. Mark every TAP as an
    // isolated bridge port: isolated ports may talk to the non-isolated host
    // bridge port, but never to one another.
    Cmd::new("bridge")
        .args(["link", "set", "dev", &tap, "isolated", "on"])
        .sudo()
        .run()
        .context("Failed to isolate TAP from peer guest ports")?;
    Cmd::new("ip")
        .args(["link", "set", &tap, "up"])
        .sudo()
        .run()
        .context("Failed to bring up TAP device")?;

    let guest_ip = inst.guest_ip();
    tracing::info!("Network configured: bridge={BRIDGE_NAME}, tap={tap}, guest={guest_ip}");
    Ok(())
}

/// Remove the instance's tap device. Tears down the bridge if no taps remain.
pub fn teardown_tap(cfg: &NetworkConfig, inst: &Instance) -> Result<()> {
    let tap = inst.tap_device();
    tracing::info!("Tearing down TAP device {tap}");

    if tap_exists(&tap) {
        Cmd::new("ip")
            .args(["link", "del", &tap])
            .sudo()
            .run()
            .context("Failed to delete TAP device")?;
    }

    // If no tap devices remain on the bridge, tear it down
    if bridge_exists() && bridge_is_empty() {
        let host_iface = resolve_host_iface(&cfg.host_iface).unwrap_or_else(|_| "eth0".into());
        teardown_bridge(&host_iface);
    }

    tracing::info!("Network teardown complete");
    Ok(())
}

/// Tear down the bridge and all NAT rules unconditionally.
pub fn teardown_all(cfg: &NetworkConfig) {
    let host_iface = resolve_host_iface(&cfg.host_iface).unwrap_or_else(|_| "eth0".into());
    teardown_bridge(&host_iface);
}

// ── Bridge management ─────────────────────────────────────────

fn ensure_bridge(cfg: &NetworkConfig, host_iface: &str) -> Result<()> {
    if bridge_exists() {
        tracing::debug!("Bridge {BRIDGE_NAME} already exists");
        return Ok(());
    }

    tracing::info!("Creating bridge {BRIDGE_NAME}");
    Cmd::new("ip")
        .args(["link", "add", BRIDGE_NAME, "type", "bridge"])
        .sudo()
        .run()
        .context("Failed to create bridge")?;

    let host_cidr = format!("{}{}", cfg.host_ip, cfg.subnet_mask);
    Cmd::new("ip")
        .args(["addr", "add", &host_cidr, "dev", BRIDGE_NAME])
        .sudo()
        .run()
        .context("Failed to assign IP to bridge")?;

    Cmd::new("ip")
        .args(["link", "set", BRIDGE_NAME, "up"])
        .sudo()
        .run()
        .context("Failed to bring up bridge")?;

    Cmd::new("sysctl")
        .args(["-w", "net.ipv4.ip_forward=1"])
        .sudo()
        .run()
        .context("Failed to enable IP forwarding")?;

    Cmd::new("iptables")
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
        .sudo()
        .run()
        .context("Failed to add NAT masquerade rule")?;

    Cmd::new("iptables")
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
        .sudo()
        .run()
        .context("Failed to add forward rule")?;

    Cmd::new("iptables")
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
        .sudo()
        .run()
        .context("Failed to add return traffic rule")?;

    Ok(())
}

fn teardown_bridge(host_iface: &str) {
    tracing::info!("Tearing down bridge {BRIDGE_NAME}");

    if let Err(e) = Cmd::new("iptables")
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
        .sudo()
        .run()
    {
        tracing::debug!("Failed to remove NAT rule (non-fatal): {e}");
    }
    if let Err(e) = Cmd::new("iptables")
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
        .sudo()
        .run()
    {
        tracing::debug!("Failed to remove forward rule (non-fatal): {e}");
    }
    if let Err(e) = Cmd::new("iptables")
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
        .sudo()
        .run()
    {
        tracing::debug!("Failed to remove return traffic rule (non-fatal): {e}");
    }

    if bridge_exists() {
        if let Err(e) = Cmd::new("ip")
            .args(["link", "set", BRIDGE_NAME, "down"])
            .sudo()
            .run()
        {
            tracing::debug!("Failed to bring down bridge (non-fatal): {e}");
        }
        if let Err(e) = Cmd::new("ip")
            .args(["link", "del", BRIDGE_NAME])
            .sudo()
            .run()
        {
            tracing::debug!("Failed to delete bridge (non-fatal): {e}");
        }
    }
}

fn bridge_exists() -> bool {
    Command::new("ip")
        .args(["link", "show", BRIDGE_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn bridge_is_empty() -> bool {
    let output = Command::new("ip")
        .args(["link", "show", "master", BRIDGE_NAME])
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => true,
    }
}

// ── Helpers ───────────────────────────────────────────────────

fn resolve_host_iface(configured: &HostInterface) -> Result<String> {
    match configured {
        HostInterface::Auto => detect_default_iface(),
        HostInterface::Named(name) => Ok(name.as_str().to_string()),
    }
}

fn detect_default_iface() -> Result<String> {
    let output = Command::new("ip")
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

fn tap_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "show", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

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
}
