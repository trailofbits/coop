use std::process::Command;

use anyhow::{Context, Result};

use crate::cmd::Cmd;
use crate::config::{HostInterface, Instance, NetworkConfig};

const BRIDGE_NAME: &str = "br0";

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
