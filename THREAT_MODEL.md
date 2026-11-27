# Threat model — coop

coop runs coding agents in Firecracker microVMs on Linux and Lima VMs on macOS.

The **VM is the isolation boundary**. Agents have broad privileges inside the guest and a guest compromise is an expected case for this model. The security goal is to keep guest control from malicious actions on the host side. 

This document identifies the principal threats and priorities; [`docs/trust-model.md`](docs/trust-model.md) records detailed invariants and accepted trade-offs, and [`SECURITY.md`](SECURITY.md) defines how to report a vulnerability.

## 1. Assets and security goals

| Asset | Security goal |
| --- | --- |
| Host account, files, and other local processes | Guest and project content cannot execute host commands or write outside an explicitly authorized destination. |
| Host credentials and coop private state | VM keys, tokens, saved environment, proxy credentials, and instance disks remain available only to their intended host user and, where explicitly forwarded, the selected guest. |
| Other VMs and host services | One guest cannot access another guest's network or state; guest-facing listeners and forwards expose only the intended service. |
| Workspace content | Transfers do not escape their destination or silently turn guest-authored files into trusted host configuration. |
| Release and build artifacts | Installation and self-update obtain the intended coop binaries and detect modified archives. |
| Maintainer accounts and CI credentials | Repository and release permissions are used only for authorized review, build, and publication. |
| Host availability | Guest-controlled output, requests, and lifecycle state cannot cause unbounded host resource use or orphan privileged processes. |

## 2. Threat actors and capabilities

| Actor | Relevant capability |
| --- | --- |
| Compromised or malicious guest agent | Controls guest files, processes, command output, network requests, and data later pulled or mounted on the host. |
| Malicious project or repository author | Supplies source files and project configuration that an agent or later host tool may read, and may influence guest-generated output. |
| Network adversary or compromised upstream service | Can interfere with downloads, API responses, DNS, or traffic reachable from the guest; cannot legitimately alter the trusted host user's config. |
| Malicious contributor or compromised maintainer | Can propose code or workflow changes; a compromised release authority can publish artifacts or change the build and verification process. |

Of note, control of the host account running coop is outside this model's isolation claim.

## 3. Risk priorities

| Scenario | Potential impact | Review priority |
| --- | --- | --- |
| Guest-controlled content reaches a privileged host operation or later host tool | Host execution, file modification, or credential exposure | Critical |
| Secret becomes available to the wrong guest | Credential theft or host data modification | Critical |
| Proxy policy or transport lets a guest misuse a host-held credential | Credential misuse or host proxy compromise | High |
| Network or VM transport lets one guest impersonate or reach another | Cross-VM access or intercepted forwarded data | High |
| Release or update verification is bypassed | Malicious installed binary | Critical |
| Guest-controlled request, output, or lifecycle failure exhausts host resources | Service disruption or unrecoverable instance state | High |

## 4. Scope and review triggers

The disclosure scope and exclusions are in [`SECURITY.md`](SECURITY.md#scope).

Review this model when a new boundary-crossing feature or secret is added.
Changes listed under [explicit confirmation](docs/trust-model.md#changes-requiring-explicit-confirmation_
need human review before merge.
