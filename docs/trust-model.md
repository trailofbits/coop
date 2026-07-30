# Trust model

This is the engineering-facing trust model for `coop` — the authoritative list
of trust boundaries, taint sources, and the invariants that hold the isolation
together. The [`review-security`](../.claude/agents/review-security.md) agent
reads this file, and the root [`CLAUDE.md`](../CLAUDE.md) "Trust model" section
points here. Apply these checks whenever you build or review a change.

This complements — it does not replace — [`SECURITY.md`](../SECURITY.md), which
is the vulnerability-disclosure policy. `SECURITY.md` tells outsiders how to
report a problem; this document tells contributors where the boundaries are so
they don't introduce one.

## The core boundary: the VM

**coop's isolation boundary is the guest VM itself** — a Firecracker microVM on
Linux, a Lima VM (Apple Virtualization.framework) on macOS. The point of the
tool is to run AI coding agents (Claude Code, Codex) with broad autonomy
*inside* that boundary, so the guest is deliberately permissive:

- The guest user has passwordless `sudo` (`NOPASSWD:ALL`).
- Claude runs with a managed `~/.claude/settings.json` carrying
  `defaultMode: bypassPermissions`; the `codex`/`claude` launchers pass
  `--dangerously-bypass-approvals-and-sandbox` / `--dangerously-skip-permissions`
  unless the user passes `--ask`.

This is intentional and correct: there is **no privilege boundary inside the
guest to protect** — the whole VM is the blast radius. The security model is
"anything the agent does stays in the VM." Every rule below exists to keep that
true: to stop guest-authored (therefore untrusted) data from escalating across
the VM boundary into host code execution, host filesystem escape, or credential
exposure.

Treat the guest as **untrusted** from the host's point of view, even though the
user launched it.

## Trust zones

| Zone | Trust | Notes |
|------|-------|-------|
| Host user + `config.toml` | Trusted | `config.toml` `cmd:` values run arbitrary `sh -c` on the host (`config.rs:resolve_cmd_value`). The config file is a host code-execution surface; only the owner should write it. |
| coop process (host) | Trusted | Holds/relays secrets, constructs guest commands, runs `iptables`/`firecracker` via `sudo`. |
| The guest VM | **Untrusted** | Agent-controlled. Anything it emits — file contents, paths, archive members, command output — is a taint source once it crosses back to the host. |
| GitHub API / model endpoints / DNS | External | `api.github.com` (PAT probe, release metadata), the model endpoint, `8.8.8.8`. Reached over the network; authenticated where applicable. |

## Taint sources (treat as untrusted)

- **Guest filesystem content pulled to the host.** `workspace.rs` `pull` /
  `tar_pipe_pull` / `rsync_pull` bring guest-authored file contents, filenames,
  and symlinks onto the host filesystem. This is the **widest guest→host
  channel** and the primary place a path-traversal or symlink escape could land.
- **Guest command output read by the host.** e.g. `check_guest_dirty` reads
  `git status --porcelain` from the guest. Today this only gates control flow /
  is printed to the user — it is never fed into `sh -c` on the host. Keep it
  that way.
- **A fetched `devcontainer.json`.** `git_repo_devcontainer.rs` /
  `devcontainer.rs` parse devcontainer JSON that may originate from a remote
  repo. Its values configure the guest; they must never reach a host `cmd:`
  evaluation or host shell.
- **Downloaded update artifacts.** `update.rs` tarball + `SHA256SUMS` from the
  release host — gated by checksum and (best-effort) Sigstore attestation.
- **OCI feature blobs.** `devcontainer_oci.rs` pulls devcontainer *Features*
  from GHCR; the install snippet runs **in the guest**, not the host.

## Secrets and how they cross into the guest

coop relays several secrets from the host into the guest: `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, `GITHUB_TOKEN`/PAT, `CLAUDE_CODE_OAUTH_TOKEN`, arbitrary
user `env_forward` entries, and the VM SSH key. The invariants:

- **Never on argv.** Secrets ride SSH `SendEnv` (env channel) or process env
  (`backend.rs:prepare_env_forwarding`, `EnvForward`), or are piped via **stdin**
  (`SshTarget::exec_with_stdin`, the git-clone credential helper
  `build_clone_with_token_script`, `curl -H @-` in `github_pat.rs`/`update.rs`).
  Never build a command line with the secret as an argument — it is visible in
  `ps`/`/proc`. Known exceptions are the macOS `security` and 1Password `op`
  backends, which take the secret on argv because their CLIs offer no stdin
  path; this is documented at the call sites and limited to the store step.
- **`GITHUB_TOKEN` defaults to Off.** It is only forwarded with an explicit
  `github = auto|env|pat` opt-in (`backend.rs:resolve_github_token`). When
  forwarded, `bootstrap_agents` runs `gh auth setup-git`, which makes the token
  **persistent guest state** (a git credential helper any guest process can
  read). A change that forwards it by default, or makes it persistent where it
  wasn't, is a finding.
- **Secret files stay `0600`, dirs `0700`.** File-backend PATs live at
  `<state_dir>/github-pat/<account>.txt` (`secret_store.rs:store_file`); all
  managed writes go through `fs_util::atomic_write_with_mode` / `atomic_write_ssh`,
  which never relax permissions.
- **Secrets stay out of logs.** `Cmd::redacted_arg` redacts argv in traces;
  `EnvForward`/`Secret<T>` custom `Debug` impls keep values out of debug output.
  Do not log a resolved secret.
- **The stored token is indirected, never inlined.** `config.toml` holds a
  `cmd:...` retrieval command (`secret_store.rs:CmdToken`), not the plaintext
  token; coop runs it at VM-start to fetch the value.

## SSH boundary

- coop connects to the guest with `StrictHostKeyChecking=no`,
  `UserKnownHostsFile=/dev/null`, `IdentitiesOnly=yes`
  (`backend.rs:SshTarget::ssh_opts`, `workspace.rs:ssh_config_block`). This is
  deliberate: guest keys are ephemeral and regenerated per VM, so there is no
  stable host key to pin. The trade-off is that a MITM on the path to the guest
  is not detected — acceptable because that path is loopback / a local TAP link
  to a VM the host itself owns.
- The guest SSH key (`<data_dir>/vm_key`, ed25519, **passphrase-less by
  design**) is a VM-access credential. Do not "harden" it with a passphrase
  (it must be used non-interactively), but do flag any change that exposes it
  or copies it off the host.
- Flag any change that extends the no-host-key-checking options to a
  **non-guest** host.

## Network

- **Port-forwards bind to `127.0.0.1` only** (`port_forward.rs`): both the
  collision probe and the `ssh -L 127.0.0.1:<host>:127.0.0.1:<guest>` spec.
  There are **no `0.0.0.0` binds** anywhere in the tree. A new listener must
  bind loopback explicitly.
- Firecracker networking (`network.rs`) sets up a `br0` bridge + per-instance
  TAP, enables `ip_forward`, and adds a `MASQUERADE` + `FORWARD` ruleset so the
  guest reaches the internet through the host's default interface. A change
  that widens guest egress or adds inbound reachability is a finding.
- `rewrite_host_url` rewrites a loopback local-model endpoint to the
  guest-visible gateway address so the guest can reach a model server running
  on the host; non-loopback URLs pass through unchanged.

## `coop update` trust chain

Self-update (`update.rs`) must preserve, in order:

1. Metadata from the pinned `trailofbits/coop` GitHub repo (compile-time const).
2. `normalize_tag` — the version tag is validated as semver **before** it enters
   the API URL path (path-traversal guard).
3. **Mandatory checksum.** The `SHA256SUMS` asset must be present (install is
   refused otherwise) and every downloaded tarball is verified against it
   (`verify_sha256`, constant-size `Sha256Hash` compare).
4. **Best-effort attestation.** `gh attestation verify --repo trailofbits/coop
   --bundle attestations.jsonl` (Sigstore provenance), against the bundle asset
   downloaded from the same release. `--bundle` makes verification offline: no
   attestations-API call, so no GitHub credential — but it does *not* weaken the
   check, because the bundle is signed and `--repo` still pins the signer
   identity, so a substituted or tampered bundle fails. Releases publishing no
   bundle asset fall back to the API path (credential required). Skipped with a
   logged note if `gh` is absent, and skipped entirely when
   `COOP_UPDATE_API_BASE_URL` is overridden (test mode). So provenance is *not*
   guaranteed on hosts without `gh` — checksum is the floor.
5. Extraction with `tar -xzf --no-same-owner --no-same-permissions` (path-escape
   safe), then an atomic `rename`-over-self.

`COOP_UPDATE_API_BASE_URL` redirects the update origin **and** disables
attestation; the checksum then only proves integrity against *that* server's own
`SHA256SUMS`, giving no provenance. Only the pinned `github.com` default +
attestation provide provenance. Flag any change that widens where that override
is honored, or that softens any step above.

## Documented, accepted trade-offs

These are deliberate and documented in [`CLAUDE.md`](../CLAUDE.md) /
[`docs/ARCHITECTURE.md`](ARCHITECTURE.md). Don't "fix" them without
understanding the rationale; do flag a change that *widens* them:

- **`DOCKER_INSECURE_NO_IPTABLES_RAW=1`** in the guest. The Firecracker CI
  kernel lacks `iptable_raw`, so Docker 28+ can't install its raw-table
  "direct access filtering" rule. Without it, other hosts on the guest's LAN
  could route to published container ports bound to loopback — irrelevant here
  because the guest's only network neighbor is the host and the VM *is* the
  isolation boundary.
- **iptables-legacy** and a **static `/etc/resolv.conf`** in the guest — both
  work around the minimal CI kernel (no nftables, no systemd-resolved). Guest
  config only; no host-side trust impact.

## Stop-and-confirm checklist

Stop and get explicit human confirmation before merging a change that:

- adds an outbound URL, a network listener, or an egress/`FORWARD`/NAT rule —
  name the boundary it crosses and what authenticates it;
- forwards a new secret into the guest, or makes an existing one persistent
  inside the guest;
- runs a host subprocess on tainted (guest- or fetch-derived) bytes — no shell
  strings; use `Cmd::arg`/`RemoteCommand::arg`;
- writes a **host** filesystem path derived from tainted data — validate against
  traversal first;
- logs or traces tainted or secret content — that channel becomes an
  exfiltration path;
- softens any step of the `coop update` verification chain.
