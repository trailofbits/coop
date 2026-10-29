# Firecracker network modes

Linux/Firecracker instances have a persistent network mode. `open` is the
default and preserves the normal virtio NIC, per-instance TAP, shared bridge,
forwarding, and NAT. `isolated` starts Firecracker with no network interface and
creates no TAP for that boot. The VM must be stopped to change modes.

```sh
coop network my-vm
coop network my-vm isolated
coop network my-vm open --restart
coop network my-vm isolated --restart
```

`--restart` explicitly acknowledges that active guest processes and agent
sessions are terminated. A failed start leaves the VM stopped with the requested
mode selected; coop does not silently fall back to open networking. Opening a VM
grants general network access to every guest process, not only the installer or
command that motivated the change.

An isolated VM keeps coop control through OpenSSH over Firecracker vsock. Shell,
exec, SCP, rsync, workspace sync, generated SSH configuration, local port
forwarding, status, logs, and graceful shutdown therefore do not require a NIC.
Managed Anthropic and OpenAI credential proxies remain host-loopback services
and reach guest loopback through fixed SSH reverse forwards on the same
vsock-backed connection. Raw Anthropic, OpenAI, Codex, XAI, and GitHub
credentials are not forwarded into an isolated guest.

Claude and Codex require their corresponding managed proxy while isolated.
Configure them with `coop proxy setup --vm NAME` and
`coop proxy setup --openai --vm NAME`. Grok Build and local-model routing are
unsupported because they have no equivalent fixed proxy path. A control-only VM
is valid with `--no-agents`; a network-mode restart can use
`--restart --no-agents` for the same purpose. Direct provider auth never becomes
a fallback.

Prepare packages, remote marketplaces, plugins, and source repositories in an
image or during an explicit open maintenance boot. Pending remote agent installs
and `--git-repo` fail preflight in isolated mode. GitHub operations, remote MCP
servers, browser tools, package downloads, container pulls, agent self-updates,
and other guest IP networking are unavailable. Local workspace and marketplace
content copied from the host remains usable.

A typical maintenance cycle is:

```sh
coop network my-vm open --restart
coop exec my-vm -- sudo apt-get install PACKAGE
coop network my-vm isolated --restart
```

The mode belongs to the instance, not its image. Restore, reprovision, and commit
preserve the target instance's selection. Changing `[vm].firecracker_network`
affects new instances only. Isolation also requires a current image with the
vsock SSH listener and an instance created with `firecracker_control = "vsock"`;
coop does not silently migrate legacy TCP instances.

This mode limits guest-originated Ethernet/IP traffic; it is not a claim that
the VM is fully offline. Agent request bodies can disclose data to the selected
model provider through the managed proxy. Switching modes does not sanitize the
guest disk, remove credentials already present (including files coop copied
during an earlier open boot), or revoke existing provider credentials. A
malicious process can stage persistent code while isolated and use a later open
maintenance boot to establish durable network access that survives subsequent
reboots. Recreate from a prepared image when a clean disk or credential
boundary is required.

Lima does not implement this setting. Querying `coop network` reports it as
unsupported on macOS, and attempts to change it fail.
