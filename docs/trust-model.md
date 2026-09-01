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
- **Proxy mode keeps the model API keys out of the guest entirely** (issue #411,
  opt-in `[proxy]`). When enabled in remote model mode, `ANTHROPIC_API_KEY`
  (Claude) and/or `OPENAI_API_KEY` (Codex) are **not** forwarded
  (`prepare_env_forwarding`'s `suppress_anthropic_key`/`suppress_openai_key`, one
  per provider); the host-side `coop-proxy` holds the real credential and the
  guest gets only a per-instance capability token (Claude via `settings.json`,
  Codex via the `coop_local` provider's bearer `env_key`). In proxy mode Codex's
  `~/.codex/auth.json` is also **not** staged onto the guest disk (it holds a
  refreshable subscription token). Each credential is resolved on the host and
  handed to `coop-proxy` over **stdin**, never argv or disk; a resolution failure
  fails the boot closed. A per-VM override
  (`proxy_state.rs`, `<inst.dir>/proxy.json`) selects a different host-side
  credential for one VM — resolution is override → default → off — without
  changing the proxy binary or the capability token. See
  [`credential-proxy.md`](credential-proxy.md).
- **Codex ChatGPT account auth is persistent guest state.** With
  `[codex] auth = "chatgpt"`, coop suppresses `OPENAI_API_KEY` across config,
  process env, `env_forward`, and persisted `--env` overlays, writes
  `cli_auth_credentials_store = "keyring"` to guest `~/.codex/config.toml`, and
  excludes host `auth.json` from the guest copy. Codex stores its cached account
  credentials in the guest Linux Secret Service instead. This avoids API-key
  billing and host `auth.json` copying, but it does **not** keep the ChatGPT
  refresh token out of the guest. A compromised guest can use or extract any
  credential its keyring session can unlock. The keyring setting is written
  during agent bootstrap, so `--no-agents` skips it. On a guest where no
  earlier boot wrote it, the wrapper then falls through to plain Codex and
  `codex login` writes a plaintext `~/.codex/auth.json` instead. coop warns in
  exactly that case; the setting lives on the guest disk, so once written it
  survives a later `--no-agents` start.

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
- **The credential proxy (issue #411) binds host loopback** (`127.0.0.1`, it
  refuses an unspecified address at bind) and is exposed into the guest with a
  per-instance `ssh -R` reverse tunnel (`proxy.rs:spawn_reverse_forward`), so —
  like the port-forwards above — it never binds a non-loopback interface and is
  reachable by exactly one guest, identically on both backends. It is guarded
  by a per-instance capability token and forwards only to a fixed per-provider
  upstream (`api.anthropic.com` / `api.openai.com`), never a guest-supplied host
  — one proxy process and one tunnel per (VM, provider). Its own TLS-verifying
  outbound HTTPS is the intended egress; a change that lets the guest influence
  the upstream host, or that binds anything wider than loopback, is a finding.

- **The credential proxy is jailed (issue #411, slice 3).** `coop-proxy` holds
  the real credential and terminates connections the untrusted guest
  originates, so it is the feature's new attack surface; the jail bounds the
  blast radius of a proxy exploit. On **Linux** the proxy self-confines with
  **Landlock**, applied in `coop-proxy`'s `main` after its libraries load and
  before it binds, so every tokio worker inherits the domain. The confinement
  is tiered by kernel capability: all filesystem writes and all program `exec`
  are denied as a hard floor (kernel ≥5.13); on kernels ≥6.7 TCP `connect` is
  additionally limited to `:443` (upstream) and `:53` (DNS), `bind` to the
  listener port (see the host-kernel floor below).
  On **macOS** the launcher wraps the spawn in `sandbox-exec` with the Seatbelt
  profile in [`src/seatbelt-proxy.sb`](../src/seatbelt-proxy.sb) (deny by
  default; allow file reads, name resolution, the loopback listener, and egress
  only to `:443`/`:53`). Either way the launch is **fail-closed** on the
  high-value denials: if the filesystem-write/program-`exec` floor cannot be
  established — Linux kernel <5.13, or the macOS Seatbelt profile fails to
  apply — the proxy exits before serving, `proxy.rs`'s post-spawn readiness
  probe never connects, and the VM start aborts, so the credential-holding
  proxy never runs without that floor. Reads stay open (the resolver
  config, the dynamic linker) and writes to the already-open stderr log fd are
  unaffected, because both jails gate path opens / new connections, not
  existing descriptors.

  **Accepted limitations, by construction** (do not file these as findings; do
  flag a change that *widens* them):
  - **Port-scoped, not host-scoped.** Both Landlock and Seatbelt filter by
    port, not hostname/IP. A *fully compromised* proxy could still open a TCP
    connection to some other host on `:443`; the two upstreams' identity is
    enforced one layer up, at the proxy's TLS verification + pinned `Host`, and
    the guest still cannot retarget them. Host-scoped egress would need IP
    pinning (fragile against CDN rotation) or an L7 egress proxy — out of scope
    and consistent with the DNS/CDN caveat #2 already accepts.
  - **UDP egress is not restricted on Linux.** Landlock's network rules are
    TCP-only, and DNS needs UDP `:53`, so arbitrary UDP egress remains possible
    for a compromised proxy. Closing it would need `nftables` owner-matching
    (a dedicated uid + `sudo` + per-instance teardown) — deliberately deferred.
  - **Host-kernel floor / deprecated primitive.** The Linux jail degrades by
    kernel version rather than all-or-nothing:
    - **kernel ≥6.7** — full jail: filesystem-write + program-`exec` denied and
      TCP egress port-scoped to `:443`/`:53`;
    - **kernel 5.13–6.6** — filesystem-write + program-`exec` denied, but
      Landlock has no network rules, so TCP egress is **not** scoped (open
      egress). Acceptable because the network tier is already the weak,
      port-scoped layer and upstream identity is TLS-pinned in the proxy;
    - **kernel <5.13** — the floor cannot be established, so the proxy **fails
      closed** and refuses to start.

    `sandbox-exec` is officially deprecated but still functional and has no CLI
    successor — a pragmatic, aging primitive.

  These match the design's "Cross-platform hardening" note: the Firecracker
  (Linux) story is the stronger one; Lima (macOS) is closable to near-parity
  with Seatbelt, with the stated caveats.

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
   downloaded from the same release.

   `--bundle` means **no attestations-API call and no credential** — `gh` marks
   the flag `DisableAuthCheckFlag`, so no token or `gh auth login` is needed.
   The bundle asset is fetched with a deliberately unauthenticated request
   (`curl_download(url, dest, None)` in `update.rs`, `download_bundle` in
   `install.sh`) rather than through `download_asset` / `gh release download`,
   which would re-attach the very credential this transport exists to avoid — a
   SAML-restricted token would then 403 on the fetch and drop the chain back to
   the API path. Keep both fetches credential-free. It is *not* offline:
   without `--custom-trusted-root`, `gh` still reaches `tuf-repo.github.com`
   and the Sigstore public-good CDN for the trust root (one-day cache) and
   fails if neither is reachable.

   It does *not* weaken the check materially, but the two transports are not
   interchangeable, and the differences are worth stating rather than arguing
   away:

   - **The bundle path accepts a superset.** The API path only returns
     attestations registered in the repo's attestation store, so minting one
     requires `attestations: write`. The `--bundle` path accepts any
     correctly-signed bundle sitting in a release, which requires only
     `contents: write`.
   - **The bundle path is unrevocable.** `DELETE
     /orgs/{org}/attestations/digest/{digest}` exists, Fulcio certificates
     carry no CRL or OCSP, and nothing in `gh`'s verification path consults a
     revocation source — so a bundle already saved by a client keeps verifying
     indefinitely after the attestation is deleted.

   What still defeats a substituted bundle is the **subject-digest binding** —
   `gh` digests the artifact and requires a matching subject. `--repo
   trailofbits/coop` pins the source repository and constrains the signer SAN
   to that repo, but not to a specific workflow file or ref: any workflow on
   any ref in `trailofbits/coop` holding `id-token: write` +
   `attestations: write` mints a bundle that satisfies it. `--signer-workflow`
   / `--cert-identity` are the tighter pin and neither client passes one — the
   API path is keyed by digest against the same repo-scoped store and is
   equally unpinned there.

   A release that publishes no bundle asset — or one whose download fails, or
   whose bundle is empty — falls back to the API path, where `gh` requires a
   credential authorized for the org even though the store itself is
   anonymously readable. `update.rs` and `install.sh` treat all three
   identically, because for a client they are one situation: no bundle to read.
   An empty bundle is
   rejected rather than passed through because `gh` before 2.56.0
   (cli/cli#9541) reports success on one, having verified nothing;
   `release.yml` also fails the release rather than publish one. A bundle that
   downloads and fails to *verify* is refused outright, not retried through the
   API — that is no stricter on integrity, but a digest mismatch, a corrupt
   download and an unusable `gh` all surface here, and switching transports
   would mask them. Skipped with a logged note if `gh` is absent, and skipped
   entirely when `COOP_UPDATE_API_BASE_URL` is overridden (test mode). So
   provenance is *not* guaranteed on hosts without `gh` — checksum is the
   floor.
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
