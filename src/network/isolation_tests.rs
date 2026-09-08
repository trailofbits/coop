//! Run through tests/integration-network.sh, which supplies disposable namespaces.

use std::fs;
use std::process::Command;

use crate::network::isolate_tap_port;

fn ping(namespace: &str, address: &str) -> i32 {
    let output = Command::new("ip")
        .args([
            "netns", "exec", namespace, "ping", "-n", "-c", "1", "-W", "2", "-w", "3", address,
        ])
        .output()
        .unwrap();
    let code = output.status.code().unwrap();
    assert!(
        code == 0 || code == 1,
        "ping failed to execute normally: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    code
}

fn assert_peers_reachable() {
    assert_eq!(ping("guest-a", "192.0.2.3"), 0, "A must reach B");
    assert_eq!(ping("guest-b", "192.0.2.2"), 0, "B must reach A");
}

fn assert_gateway_reachable() {
    for guest in ["guest-a", "guest-b"] {
        assert_eq!(ping(guest, "192.0.2.1"), 0, "{guest} must reach gateway");
    }
}

#[test]
#[ignore = "requires disposable namespaces; run tests/integration-network.sh"]
fn bridge_port_isolation_blocks_peers_without_firewall() {
    let host_namespace = std::env::var("COOP_NETWORK_TEST_HOST_NS").unwrap();
    assert_ne!(
        fs::read_link("/proc/self/ns/net").unwrap().as_os_str(),
        std::ffi::OsStr::new(&host_namespace),
        "refusing to change bridge flags in the host network namespace"
    );

    // An empty ACCEPT ruleset prevents br_netfilter from masking a missing flag.
    let rules = Command::new("iptables")
        .args(["-S", "FORWARD"])
        .output()
        .unwrap();
    assert!(rules.status.success());
    assert_eq!(
        String::from_utf8_lossy(&rules.stdout).trim(),
        "-P FORWARD ACCEPT"
    );

    assert_peers_reachable();
    assert_gateway_reachable();
    isolate_tap_port("port-a").unwrap();
    // Isolation is pairwise: marking just one port must leave peers reachable.
    assert_peers_reachable();
    isolate_tap_port("port-b").unwrap();
    assert_eq!(ping("guest-a", "192.0.2.3"), 1, "A must not reach B");
    assert_eq!(ping("guest-b", "192.0.2.2"), 1, "B must not reach A");
    assert_gateway_reachable();

    // Recovery rules out endpoint failure as the reason for the negative probes.
    let status = Command::new("bridge")
        .args(["link", "set", "dev", "port-a", "isolated", "off"])
        .status()
        .unwrap();
    assert!(status.success());
    assert_peers_reachable();
}
