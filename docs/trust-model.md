# Trust model

This document states coop's security boundaries and the rules contributors must
preserve. [`THREAT_MODEL.md`](../THREAT_MODEL.md) lists prioritized threat
scenarios; [`SECURITY.md`](../SECURITY.md) gives the disclosure process.

## The core boundary: the VM

The **guest VM is the isolation boundary**: Firecracker on Linux and Lima on
macOS. Coding agents have broad permissions inside it, including passwordless
`sudo` and default bypass modes. Treat the guest as compromised from the host's
point of view. Guest-controlled files, output, network requests, and disk state
must not become host code execution, filesystem escape, or unapproved credential
access. Explicitly configured mounts, transfers, forwards, and host services
cross this boundary; the VM does not make their data safe for host tools.

## Trust zones

| Zone | Trust assumption |
| --- | --- |
| Host user and `config.toml` | Trusted. `cmd:` values execute host shell commands; only the owner should write this file. |
| Local or fetched project files | Untrusted. Project opt-in grants guest configuration, not host execution authority. |
| coop host process | Trusted to hold secrets, launch host tools, and manage VM state. Privileged Linux operations require special scrutiny. |
| Guest VM | Untrusted, including agent output, paths, archive members, and transferred files. |
| GitHub, model providers, DNS, and downloads | External inputs, authenticated or integrity-checked where the operation requires it. |

## Taint sources (treat as untrusted)

- **Pulled files and live mounts.** Guest-authored names, symlinks, contents,
  and metadata can affect host paths and tools. Pull staging and `.git` filters
  are defense in depth; transferred files remain untrusted when a host tool
  later discovers them. A repository pulled by an older vulnerable release
  needs independent host inspection. See [workspaces](workspaces.md).
- **Guest disks and output.** `coop commit` can make modified disk contents a
  future image template. Guest command output is data, never host shell source.
  Linux setup and disk operations must preserve the checked filesystem boundary.
- **Project and fetched configuration.** Follow values through translation,
  merging, saved state, and later host consumers; a guest configuration opt-in
  does not authorize host interpretation.
- **Update artifacts.** Release metadata, archives, checksums, and attestation
  bundles are external inputs subject to the update chain below.

## Host subprocess boundary

For every less-trusted input reaching a host process, inspect the *whole* launch:
executable lookup, argv and option parsing, environment, cwd, inherited file
descriptors, and files or wrappers the tool reads implicitly. Follow the value
through persisted state and unchanged later consumers. An argv API prevents
shell interpolation, but a tainted `PATH`, loader variable, tool config, or
transferred file may still change host behavior. Path containment and transfer
opt-in do not authorize execution.

Coop-owned SSH clients and Linux setup/network tools resolve validated absolute
executables and use bounded launch environments. Linux network overrides are
exact paths with no fallback; path-chain checks establish filesystem control,
not package identity or command semantics. Validation is pathname based and
does not pin the executable inode or loader closure. Setup's package manager
still delegates to trusted host package policy. See [host tool configuration](configuration.md#exact-network-host-tool-paths)
and [backend setup](backends.md).

Guest-bound environment variables use generated `COOP_SSH_ENV_*` aliases until
a guest shell restores their intended names. Missing aliases or failed guest
exports stop the command. Never pass the guest environment map to a host
subprocess or let those aliases configure a host SSH wrapper.

## Secrets and how they cross into the guest

- **Explicit exposure.** `[[guest_files]]` copies selected host source trees,
  including any secrets within them. Staging rejects unsafe source entries and
  guest destinations; the selected content is still intentionally exposed.
  Writable Lima mounts likewise expose their selected host trees. See
  [guest files](configuration.md#guest-files) and [private host storage](configuration.md#private-host-storage).
- **Transport.** Managed model keys, GitHub credentials, custom environment,
  and other secrets travel via process environment, SSH `SendEnv`, or stdin,
  not argv. The macOS `security` and 1Password `op` store commands are documented
  argv exceptions. Redact resolved values from logs and debug output. Host
  `cmd:` resolvers are trusted commands, executed at VM start.
- **GitHub.** Managed `GITHUB_TOKEN` forwarding defaults to off and requires
  explicit configuration or a saved PAT binding. `--no-github` suppresses the
  managed assignment for that invocation. Bootstrap may save a forwarded token
  as guest Git credential state; later starts do not erase it. See
  [GitHub auth](configuration.md#github-auth).
- **Private state.** Managed directories and sensitive files use owner-only
  permissions (`0700` and `0600` respectively), with symlink, hardlink, and
  path checks at sensitive boundaries. Lima rejects writable mounts covering
  coop config, data, secrets, or control state before boot; a path check cannot
  prove that a previously exposed file is clean.
- **Proxy mode.** The opt-in host proxy keeps selected model credentials and
  Codex host `auth.json` out of the guest. The guest receives a per-instance
  capability that still authorizes allowed model operations. The real secret
  reaches the proxy via stdin. Resolution failure aborts boot. A per-VM
  override can select a different host credential. See
  [credential proxy](credential-proxy.md).
- **Codex ChatGPT mode.** Guest keyring storage avoids copying host `auth.json`
  but leaves a refreshable credential accessible to the guest. Bootstrap removes
  stale guest `auth.json`; the managed launcher refuses a missing keyring
  setting. `--no-agents` can skip first-boot setup, so it is not a credential
  isolation control. See [Codex integration](codex-integration.md).

## SSH boundary

Coop-owned SSH, SCP, and rsync clients use validated executables, clear their
host environment, disable ambient SSH config, agent use, and agent forwarding,
and permit only call-site forwards. A user-run `ssh coop-<name>` is outside that
managed launch contract. Exact host-tool overrides still trust the selected
executable's semantics and dynamic dependencies.

Guest host-key checking is disabled because VM keys are ephemeral. This is a
local-path trade-off: traffic diverted to an impostor guest could be
intercepted, including `SendEnv` data. Never extend this exception to a
non-guest host. The passphrase-less VM-access private key stays in private host
storage and must not be copied into the guest.

Firecracker vsock mode uses a bounded `CONNECT` handshake and a trusted
per-instance socket path; it does not fall back to TCP. Its jailer runs a
verified, unprivileged Firecracker process under a private host-user directory.
Unknown runtime state and uncertain live sockets block cleanup rather than
being killed or unlinked by guesswork. Guest-influenced serial output goes to
the system journal, whose readers may include users other than the VM owner.
This wider readership requires explicit confirmation before merge; `coop logs`
saves a bounded stopped snapshot. See [backend lifecycle](backends.md)
and [Firecracker modes](firecracker-network-modes.md).

## Network

Host port forwards and the credential proxy bind loopback. Firecracker `open`
mode uses a TAP, bridge, NAT, and host forwarding; guests can reach the host for
intended services. Guest-to-guest IP isolation requires both isolated bridge
ports and a first-position `FORWARD` drop rule, asserted at startup. Residuals
include pre-existing unisolated bridge members, guest address impersonation,
IPv6, and firewall reloads. Lima templates use separate per-instance usernet
stacks; adding shared networks changes that boundary. See [backend networking](backends.md#tap-networking-open-mode).

Firecracker `isolated` mode starts without a NIC and uses vsock for control and
selected proxy forwards. It prevents guest Ethernet/IP egress through coop,
but model bodies can still leave through the host proxy and old guest-disk
secrets remain. See [network modes](firecracker-network-modes.md).

The host credential proxy accepts only a per-instance capability and allowed
operations to fixed provider hosts over TLS. Its Linux Landlock or macOS
Seatbelt jail must deny filesystem writes and program execution before serving.
Network restrictions are port scoped, and older supported Linux kernels have
weaker TCP restrictions; Linux UDP remains unrestricted. A proxy compromise
can therefore still use some outbound paths. See [proxy policy and limits](credential-proxy.md#what-it-does-and-does-not-guarantee).

## Browser editor transport

`coop editor --web` runs code-server, extensions, and terminals in the VM.
Password login limits editor access, and the host browser uses a fresh
session-specific `.localhost` origin for guest-controlled content. Browser
vulnerabilities remain possible. See [editor](editor.md#browser-editing).

## `coop update` trust chain

Both the CLI updater and `install.sh` need review when this chain changes:

1. Bind metadata to the fixed `trailofbits/coop` release identity and a
   canonical `v`-prefixed semver tag. Bind each asset URL to that tag and exact
   expected filename before download; reject ambiguous identities.
2. Require `SHA256SUMS` and verify the archive digest before extraction.
3. When `gh` is present, verify artifact provenance against the release
   workflow at the selected tag and source ref, on a GitHub-hosted runner.
   Prefer the published bundle; an absent or empty bundle may fall back to the
   digest-keyed attestations API. Verification or parsing failure aborts the
   install. Without `gh`, provenance is skipped, so checksum is the floor.
4. Use HTTPS-only download and redirect protocols, reject archive path escape,
   and atomically replace the installed binary. The CLI and proxy binary are
   each replaced atomically, not as one transaction.

The attestation pins a tag name, not an immutable commit. Bundle verification
accepts a correctly signed bundle published with release-content permission
and cannot revoke one already obtained by a client; API verification has a
narrower publication path. Neither path authenticates every upstream build
input. The separately compiled test source does not make production update
origins runtime-selectable. See [update security details](update-security.md)
and [architecture](ARCHITECTURE.md#coop-update).

## Documented, accepted trade-offs

- Native agent installers are trusted build inputs. Firecracker provisioning
  runs them in a host-side chroot, which is not VM isolation; Lima provisioning
  and live updates run inside a VM.
- `DOCKER_INSECURE_NO_IPTABLES_RAW=1`, `iptables-legacy`, and a static guest
  resolver are guest-side workarounds for the minimal Firecracker CI kernel.
  See [platform notes](platform-notes.md).
- Deliberate guest host access, disabled guest host-key checking, proxy
  capability use, and proxy network limits are described above. A change that
  widens any of them requires review.

## Changes requiring explicit confirmation

Before merge, obtain explicit human confirmation if a change:

- Adds an outbound URL, listener, egress rule, `FORWARD` rule, or NAT rule.
- Forwards a new secret into the guest or makes an existing one persistent.
- Passes guest, project, or fetched data into a host subprocess; inspect its
  executable, environment, cwd, configuration, and arguments.
- Derives a host filesystem path from tainted data.
- Logs or traces tainted or secret content.
- Weakens any step of the `coop update` verification chain.
