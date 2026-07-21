# Design: Issue #411 — credential non-exposure via a host-side injecting proxy

**Status:** proposal · **Scope:** the credential-injection mechanism and its Claude↔Codex parity; egress filtering (#2) and subscription-token injection are adjacent, treated only where they touch this seam
**Author:** design pass for #411 · **Date:** 2026-07-16

---

## 0. TL;DR

- **Build it host-side, per-integration, not as a helper VM and not (yet) as a
  general MITM proxy.** The secret already lives on the host today (resolved via
  `cmd:` and forwarded in); a host-side injecting proxy keeps it exactly where it
  is and simply stops forwarding it inward. A helper VM is the *correct* home only
  for a general arbitrary-upstream proxy, and only on Firecracker — see §3.
- **The per-integration proxy needs no MITM CA.** Because the guest is
  *explicitly* pointed at the proxy via `ANTHROPIC_BASE_URL` /
  `[model_providers.*].base_url`, the guest→proxy hop is a configured endpoint on
  the private host-guest link (plain HTTP), and only the proxy→upstream hop is
  TLS. The "guest-trusted MITM CA" the issue flags is a *general-proxy* cost, not
  a per-integration one (§6). This is the single biggest reason to start
  per-integration.
- **Claude↔Codex parity is clean for the API-key path and reuses config surfaces
  coop already writes** (`claude_env_block`, `codex_local_config`). Both agents
  support a base-URL override plus a "guest holds no real credential" disposition;
  the proxy injects the real key upstream. See §5.
- **Subscription auth splits by vendor, and it collapses to "inject a static
  token" — no OAuth-refresh code (§7).** Claude ships an official one-year
  `setup-token`, so the proxy injects it like any bearer (this is also #62's
  non-exposure answer). Codex ships *no* long-lived token — only a client-refreshed
  `auth.json` that OpenAI itself steers away from for automation — so Codex
  subscription is out of scope for the proxy and routes to API-key mode. Both
  agents get full non-exposure via API keys; only Codex-subscription would have
  required refresh logic, and it is deliberately not built. v1 also **stops staging
  `~/.codex/auth.json` onto the guest disk** (`backend.rs:2326`) when proxy mode is
  on, closing an existing at-rest exposure.
- **Ship order:** Anthropic first (the `ANTHROPIC_BASE_URL` rewrite seam already
  exists), Codex second (the `[model_providers.coop_local]` seam already exists),
  GitHub later and separately (it carries the #73 GraphQL-bypass caveat and has no
  base-URL override — §8).

---

## 1. What "credential non-exposure" must guarantee

The property #411 asks for: **a prompt-injected or rogue agent inside the guest
cannot read a usable upstream credential.** Today it can — every path lands a raw
secret in the guest:

- `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GITHUB_TOKEN` arrive as SSH `SendEnv`
  environment variables, present in the agent's own process environment
  (`backend.rs:1356-1410`).
- The local-model Claude token rides inside the guest's `~/.claude/settings.json`
  (`backend.rs:1841-1858`).
- **Codex's subscription credential `~/.codex/auth.json` is copied verbatim onto
  the guest disk** (`backend.rs:2326`, `CODEX_ALLOWED_FILES`).

The VM boundary does not help here: egress is open today (#2), so a token the
agent can read is a token it can exfiltrate. Non-exposure means the credential
never enters the guest in usable form — the host (or a host-controlled boundary)
holds it and attaches it to upstream requests the guest cannot see.

What non-exposure explicitly does **not** claim (see §9): it is not *scope*
enforcement (#73) and not *egress* control (#2). It reduces what a compromised
agent can steal; it composes with, but does not replace, those two.

---

## 2. What exists today (the seams this reuses)

The exploration behind this doc established the following (file:line verified):

1. **There is no host-side proxy process today.** Local-model mode rewrites the
   base URL to point the guest *directly* at a user-run server on the host
   (`network::rewrite_host_url`, `network.rs:22-43`). coop is not in the data
   path. So #411 is net-new plumbing, not "flip on the existing relay."

2. **The bind point is free.** The guest already reaches the host at a
   backend-specific gateway: `network.host_ip` (default `172.16.0.1`) on
   Firecracker (`backend.rs:1104-1107`), `host.lima.internal` on Lima
   (`backend.rs:1287-1290`, `lima.rs:27`), both surfaced through
   `VmBackend::guest_host_address` (`backend.rs:869`). Any host listener on that
   address is reachable from the guest with zero new networking.

3. **Both agents already accept a base-URL override that coop writes.**
   - Claude: `claude_env_block` (`model_state.rs:175-197`) writes
     `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` into the managed
     `settings.json`.
   - Codex: `codex_local_config` (`model_state.rs:205-238`) writes a
     `[model_providers.coop_local]` block with `base_url`, `wire_api =
     "responses"`, `env_key = "COOP_LOCAL_API_KEY"`, merged into
     `~/.codex/config.toml` by a read-merge-write that preserves user/guest config
     (`stage_codex_files`, `backend.rs:2351-2453`).

4. **A "dummy token" convention already exists.** `LOCAL_MODEL_AUTH_FALLBACK =
   "coop-local"` (`config.rs:1598`) is the placeholder token used when a local
   endpoint needs no real credential. The proxy design generalizes exactly this:
   the guest gets a dummy (or nothing), the proxy holds the real key.

5. **Port forwarding is the wrong direction but the right pattern.**
   `port_forward.rs` uses `ssh -L` (host→guest); a proxy needs the guest to reach
   a host listener (already available via the gateway, item 2). The module's
   control-socket lifecycle pattern (`forwards.sock`, `-O exit`) is reusable for
   managing a per-instance proxy process.

The upshot: the guest-config half of this feature is **already built** for the
two model APIs. The net-new work is the host-side proxy process and its lifecycle.

---

## 3. Architecture: host-side process vs. helper VM

This is the decision the user raised. It is *not* primarily about where the secret
rests (the host already holds every secret). It is about **where the proxy's
attack surface lands when the untrusted guest attacks it** — the proxy is the one
new component that terminates a connection and parses HTTP originated by the guest.

| | Host-side process | Helper VM |
|---|---|---|
| Blast radius of a proxy exploit | **The whole host** — arbitrary code-exec on the user's machine: every other secret, SSH keys, ability to tamper with coop | **A disposable VM** — the guest gains the injected credentials + egress (which it was spending anyway) but still has no path to the host |
| Secret location vs. today | unchanged (host) | moved into a new VM |
| New machinery | one host process + lifecycle | a second VM image, boot, inter-VM networking, resource cost |
| Firecracker fit | good (can additionally jail: uid/netns, jailer already present) | natural and cheap (microVMs are the point) |
| Lima/macOS fit | good (host process on `host.lima.internal`) | **awkward** — no first-class inter-VM networking, second Virtualization.framework VM is heavy against the #404 memory floor |
| Composes with #2 "no route out" | via a single host-local pinhole | as the sole egress terminus |

The helper VM genuinely downgrades the worst case from **host compromise** to
**credential + egress compromise** — a real, principled win, and coop's founding
move ("the VM is the boundary") applied a second time. But it is justified only
when the proxy's attack surface is large, and it splits the backends: clean on
Firecracker, fighting-the-platform on Lima, which violates the `backend.rs`
shared-abstraction contract coop leans on.

**Recommendation: host-side.** For the per-integration proxy the attack surface is
small — a reverse proxy to *known* HTTPS upstreams on a mature stack
(hyper/rustls), fed a request the guest composed. On Firecracker, jail it
(separate uid + network namespace) to claw back most of the host-exposure con
without a whole VM; the jailer machinery already exists. Reach for the helper VM
**only** if coop commits to the general MITM proxy (§4), where terminating TLS for
arbitrary upstreams behind a guest-trusted CA is a large enough surface to deserve
its own boundary — and accept at that point that it is a Firecracker-first feature.

---

## 4. General proxy vs. per-integration (the issue's open question)

The two axes correlate, so decide them together:

| | Per-integration (recommended v1) | General MITM proxy |
|---|---|---|
| Coverage | the integrations we wire (model APIs, then GitHub) | every upstream uniformly |
| TLS | **none to terminate** — guest is explicitly pointed at the proxy via `base_url`; guest→proxy is plain HTTP on the private link, proxy→upstream is normal HTTPS | must terminate TLS for arbitrary hosts → **guest-trusted MITM CA to mint, install, and manage** |
| Per-service rules | one small injection rule per integration | a rule engine + CA + cert cache |
| Home (§3) | host-side | helper VM |
| Gap | any integration without a bespoke path still hands the agent a raw key | none, but far bigger build |

The decisive asymmetry is the **MITM CA**. A general transparent proxy has to
impersonate `api.anthropic.com`, `github.com`, `registry.npmjs.org`, … to the
guest, which means a coop CA the guest trusts — a new long-lived root credential
to manage and a new thing that, if mis-scoped, breaks TLS trust in the guest. The
per-integration proxy sidesteps this entirely: the guest connects to a *declared*
proxy endpoint, not to an intercepted hostname, so there is nothing to
impersonate.

**Recommendation: per-integration, host-side.** It is incremental, each slice is
independently shippable, it reuses seams that already exist (§2), and it avoids the
CA. Its one real cost — gaps for unwired integrations — is acceptable because the
highest-value credentials (the two model API keys) are exactly the ones with a
first-class base-URL override, and GitHub is separately addressable (§8). Revisit
the general proxy only if the gap list grows to where a uniform interceptor earns
its CA-management cost.

---

## 5. Feature parity: Claude ↔ Codex

Parity is a first-class requirement: Codex is a co-equal agent in coop. The
API-key path maps symmetrically onto both, reusing config surfaces coop already
generates.

| | Claude Code | Codex |
|---|---|---|
| Point at proxy | `ANTHROPIC_BASE_URL` in managed `settings.json` (`claude_env_block`) | `[model_providers.coop].base_url` in `~/.codex/config.toml` (`codex_local_config`) |
| Guest-held credential | dummy `ANTHROPIC_AUTH_TOKEN` (`coop-local` convention) | **none** — omit `env_key` and `requires_openai_auth` (Codex's documented "no auth" disposition for a custom provider) |
| Wire protocol | Messages API + SSE + tool-use round-trips | Responses API (`wire_api = "responses"`) + SSE |
| Proxy injects upstream | real key → `Authorization`/`x-api-key` → `api.anthropic.com` | real key → `Authorization: Bearer` → `api.openai.com` |

Two facts from the Codex docs make this work and are worth pinning:

- **A custom `[model_providers.*]` provider can send no credential at all.** Codex
  supports three dispositions: `requires_openai_auth = true` (use the ChatGPT/API
  OpenAI token, `env_key` ignored), `env_key = "VAR"` (send that var as Bearer),
  or **neither → no auth sent**. The third is what proxy mode uses: the guest
  holds nothing, the proxy injects. (Codex also supports `http_headers` /
  `env_http_headers` and a command-backed `[model_providers.<id>.auth]` refresh
  hook — not needed for v1 but relevant to §7.)
- **The proxy stays protocol-agnostic.** It is a streaming reverse proxy that
  injects an auth header per route and passes bytes through; it never parses
  Messages-API vs Responses-API bodies. `wire_api` and endpoint paths are the
  guest config's concern. This is *why one proxy covers both* — parity costs one
  injection rule per upstream, not a second protocol implementation.

**Parity verdict:** for the API-key path, full parity at every step, and most of
it is already coded (both base-URL writers exist; the dummy-token convention
exists). The proxy is the only shared new component.

---

## 6. The proxy: shape and binding

- **Type:** an explicit forward/reverse proxy, not a transparent interceptor. One
  host process per running instance (or one shared process keyed by instance —
  decide in the spike; per-instance is simpler to reason about and matches the
  `forwards.sock` lifecycle pattern).
- **Listener:** binds the guest-visible gateway (`guest_host_address`) on a
  loopback-scoped port. On Firecracker that is the bridge IP `172.16.0.1`; on Lima
  the host side of `host.lima.internal`. Reachable from exactly one guest, not the
  LAN.
- **Guest→proxy hop:** plain HTTP over the private host-guest link. No TLS, no CA.
  The link is already private; #2's "no route out" can make it the *only* link.
- **Proxy→upstream hop:** normal outbound HTTPS to the pinned upstream
  (`api.anthropic.com` / `api.openai.com`), with the real credential injected as
  the upstream's expected header. The upstream host is fixed per route, not taken
  from the guest — so the guest cannot retarget the injected key at an
  attacker-controlled host.
- **Streaming:** must stream request and response bodies (SSE, tool-use
  round-trips) rather than buffer — a hard requirement for both agents. A mature
  async HTTP stack (hyper) handles this; the proxy adds a header and copies byte
  streams.
- **Secret resolution:** the real key is resolved on the host at proxy start via
  the existing `resolve_cmd_value` / secret-store machinery (`config.rs:34+`,
  `secret_store.rs`) — no new secret-handling code, and the `cmd:` indirection
  keeps plaintext off disk.
- **Guest config in proxy mode:** reuse `claude_env_block` / `codex_local_config`
  with `base_url` = the proxy endpoint and the credential slot set to a per-instance
  capability token (below). This is the same read-merge-write that already lands
  local-model config (`stage_codex_files`), so it preserves user/guest config and
  cleans up on mode switch.
- **The "dummy" token is a real per-instance capability.** Do not hand the guest a
  fixed placeholder. Mint a random token at proxy start, give it to the guest as
  `ANTHROPIC_AUTH_TOKEN` / the Codex provider bearer, and have the proxy require it
  before injecting the real credential. This closes a real hole: a host-bound
  listener is reachable by *any* local process, so without a check any process
  could spend the user's account through the proxy. The capability is worthless off
  the host (it only authorizes the local proxy, which injects the real key itself),
  so exfiltrating it gains the agent nothing. Bind the listener to the guest-only
  interface (bridge IP / the host side of `host.lima.internal`), never `0.0.0.0`;
  on Firecracker, vsock is a stronger point-to-point option worth evaluating.

---

## 6a. Implementation: reuse vs. build

**Grounding fact:** coop today has *no* HTTP client, *no* async runtime, and *no*
TLS stack — `Cargo.toml` pulls only `url` among the relevant crates; the codebase
is synchronous and subprocess-oriented (`Cmd`, ssh, scp). Any proxy therefore
introduces a new async/HTTP/TLS dependency footprint into a deliberately lean,
security-critical tool. That, plus coop's single-binary distribution (it cross-
compiles and ships one artifact; it does not ask users to install and manage
daemons), drives the decision.

**Recommendation: build a minimal, purpose-built reverse proxy in Rust as its own
workspace binary** (e.g. `coop-proxy`), which coop spawns and supervises like it
already spawns ssh. Build it on the *audited primitives* — `hyper` (HTTP/1.1 +
HTTP/2 + streaming bodies) and `rustls` (pure-Rust TLS, no C/boringssl build) on
`tokio` — and own only the thin coop-specific policy glue: route → fixed upstream,
inject one header, verify the capability token, stream bytes. Isolating it in a
separate binary keeps the main CLI's dependency surface unchanged, makes the
security-critical component independently auditable, and gives the Firecracker jail
(§3, slice 3) a natural unit to confine (separate uid/netns).

Rejected alternatives, with reasons weighted for a security-critical host process
reachable by the untrusted guest and holding the raw credential:

| Option | Why not |
|---|---|
| **LiteLLM proxy** (Python) | Ships a Python runtime + a multi-tenant gateway (DB, virtual keys, admin API) — enormous attack + supply-chain surface for a two-route header injector; breaks single-binary distribution |
| **oauth2-proxy** (Go) | Wrong direction — it authenticates *inbound* users to a protected app and injects *identity* headers, not *outbound* upstream API credentials; recent header-smuggling CVEs in exactly the injection path we'd rely on |
| **Envoy / nginx** | External daemon to install, configure, and keep patched on every user's host; C/C++ + a config language as attack surface; breaks single-binary distribution |
| **Pingora** (Rust framework) | Closest reasonable option, but a full load-balancer framework (routing, health checks, LB) with a large tree incl. boringssl (C build) — more surface than a fixed 2-route injector needs. Revisit *only* if coop builds the general MITM proxy (§4), where its TLS-termination/routing machinery would earn its keep |

Non-negotiable either way: **do not hand-roll TLS, HTTP, or SSE framing.** Those
are the dangerous primitives; reuse the audited crates for them. What we write is
only the small, fixed policy layer — which is precisely why a purpose-built proxy
is *smaller* attack surface here than adopting a general framework, not larger.

---

## 7. Auth modes: login flow, renewal, and official support

The spike in §11.1 is resolved. The key result: **for every path except Codex
subscription, the proxy holds a *static, long-lived* credential and injects it —
there is no OAuth-refresh logic to write.** That is the security win, and it comes
from vendor-official mechanisms, not reverse engineering.

| Mode | User setup (login flow) | Re-auth cadence | Auto-renews? | Officially supported? |
|---|---|---|---|---|
| **API key** (both agents) | none — supply the key via `cmd:` (Keychain / 1Password / 0600 file), exactly as coop resolves secrets today | never (until the user rotates the key) | n/a — no expiry | **Yes, fully.** Anthropic documents proxy routing via `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` ("routing through an LLM gateway or proxy"); Codex documents custom `[model_providers.*]` with a Bearer `env_key` |
| **Claude subscription** | run `claude setup-token` **once** on the host → stash the printed 1-year token → reference via `cmd:` | once a year (re-run `setup-token`) | no, but the token lives one year | **Yes** — `setup-token` is the documented headless path; the token is "scoped to inference only" |
| **Codex subscription** | `codex login` on the host, then coop must hold the refreshable `~/.codex/auth.json` | session goes stale after **~8 days** without a refresh | yes, but **client-side only** (Codex refreshes on use / on 401 and writes back to `auth.json`) | **Discouraged** — OpenAI: "API keys are still the recommended option for most CI/CD jobs"; there is **no long-lived headless token**, and the refresh endpoints are internal |

**Consequences for the proxy:**

- **API-key mode is the default and needs no login flow at all** — it is the
  existing `cmd:` secret resolution, now terminating at the proxy instead of the
  guest. Full Claude↔Codex parity.
- **Claude subscription is cleanly supportable** *because Anthropic ships a
  one-year token.* The proxy holds a static bearer and injects it; renewal is a
  once-a-year manual `setup-token`. No refresh loop, no undocumented endpoints.
  This also finally gives #62 a non-exposure answer (the token never enters the
  guest).
- **Codex subscription is the one path the proxy should *not* try to serve**, and
  the blocker is OpenAI's design, not coop's: there is no long-lived token, and the
  only credential is a refreshable `auth.json` whose refresh is a *client-side*
  loop against internal endpoints that writes state back to the file. To keep the
  guest from holding it, the proxy would have to reimplement that undocumented
  refresh and own the rotating token — exactly the fragile, security-critical
  machinery a security product should not build against an unstable contract.
  OpenAI itself steers automation to API keys. **So: Codex subscription → use
  API-key mode.** This is not a parity regression coop introduces; both agents get
  full non-exposure via API keys, and Claude can *additionally* offer subscription
  non-exposure only because Anthropic ships the token for it.

**The `auth.json` exposure, closed in v1.** coop copies `~/.codex/auth.json` onto
the guest disk today (`backend.rs:2326`) — a refreshable token at rest, the worst
case #411 names. When proxy mode is on, **stop staging `auth.json`** (drop it from
the effective `CODEX_ALLOWED_FILES` for that instance). A user who wants Codex
subscription billing opts out of proxy mode explicitly and accepts the
disk-resident token — a documented, deliberate trade, not a silent one.

**On `apiKeyHelper` (Claude) as an alternative — and why it is not one.** Claude
Code documents `apiKeyHelper`, a guest-side script it calls to fetch a token
(every 5 min or on 401). It is tempting but it is *not* non-exposure: the helper
returns the token to the in-guest CLI, so the raw credential still lands in the
guest. It reduces persistence, not readability. The proxy (guest holds only the
capability token, §6) is the actual non-exposure mechanism.

### 7a. The "configure once, every VM, for a long time" property — exploit it, with guardrails

A consequence of injection is that **one host-held credential serves every VM,
present and future, with no per-VM login.** For Claude this is especially stark: a
single `claude setup-token` yields a one-year credential, so the user logs in once
and every VM gets Claude for a year with zero in-guest auth. (It is not unique to
Claude — any host-held static credential the proxy injects, including both agents'
API keys, has the same shape; the Claude subscription token just extends it to
subscription billing.) This directly resolves #62's pain (re-auth in every guest).

**Recommendation: exploit it — it is the headline UX win — and it is safe because
the credential never enters a guest.** A compromised guest cannot steal the token;
the worst it can do is *use* Claude inference while its VM is running and routed
through the proxy. Three guardrails keep the convenience from becoming a
concentration risk:

1. **Stay on the *explicit* token path.** "One login for everything" must mean the
   user deliberately runs `setup-token` (or supplies an API key) via `cmd:` — never
   coop silently harvesting the host's live interactive `/login` session
   (`~/.claude/.credentials.json` / Keychain). #62 already rejected implicit
   credential reads as supply-chain magic; that judgment holds here.
2. **Keep per-VM control at the capability-token layer, not the upstream
   credential.** The shared upstream token is the convenience; each VM's *access to
   the proxy* is a distinct, per-instance, revocable capability token (§6). So
   revocation and isolation granularity survive without per-VM logins: kill or
   rotate one VM's capability and that VM loses Claude access immediately, while the
   upstream token — and every other VM — is untouched. The single upstream login
   must **not** collapse into a single shared capability token.
3. **Treat the proxy/host as the concentrated trust root it now is.** One
   long-lived, subscription-wide credential reachable by every VM means a
   proxy-or-host compromise yields up to a year of inference abuse. That is bounded
   — Anthropic scopes `setup-token` to *inference only* (it cannot establish Remote
   Control), so the ceiling is cost/quota abuse, not account takeover — but it is
   real, and it is why the Tier 1 hardening (§14) and the Firecracker jail are not
   optional polish. The proxy is also the natural place to observe/meter concurrent
   multi-VM use.

**Do not "harden" it in the wrong place.** Artificially shortening the *upstream*
token (forcing frequent re-logins) fights the convenience for little gain, since
that token never enters a guest. Put the short-lived, ephemeral, per-session
property where it actually reduces blast radius — the per-VM capability token — and
let the upstream credential be as long-lived as the vendor allows.

---

## 8. Implementation slices

Ordered, independently shippable:

1. **Proxy core + Anthropic.** The host-side streaming reverse proxy as its own
   workspace binary (`coop-proxy`, hyper + rustls + tokio — §6a), its per-instance
   lifecycle (model the control-socket pattern from `port_forward.rs`), binding on
   `guest_host_address`, secret resolution via `resolve_cmd_value`, and
   capability-token verification (§6). Guest config: `claude_env_block` with
   `base_url` = proxy and `ANTHROPIC_AUTH_TOKEN` = the per-instance capability
   token. Covers both Claude modes at once: API key *and* the `setup-token`
   subscription token are static bearers the proxy injects (§7) — no extra slice.
   Smallest end-to-end slice because the base-URL seam already exists.
2. **Codex (API key).** Add the injection route for `api.openai.com`. Guest config:
   `codex_local_config`-style block with `base_url` = proxy and *no* `env_key` /
   `requires_openai_auth`. Stop staging `auth.json` when proxy mode is on (§7).
   Delivers Claude↔Codex parity at the API-key tier. Codex subscription is out of
   scope by vendor design (§7).
3. **Jailing (shipped).** Bound a proxy-exploit blast radius by confining
   `coop-proxy`. **Implemented with Landlock (ABI v4), not the uid+netns jailer
   sketched in §3/§11.3.** The settled architecture binds the listener on host
   `127.0.0.1` reached via `ssh -R`, and an isolated network namespace gets its
   own loopback the host-side tunnel could not reach — so instead of moving the
   bind, the proxy self-confines with Landlock (filesystem-write + `exec`
   denied; TCP egress limited to `:443`/`:53`) applied before it binds. macOS,
   which has no in-process sandbox, is confined externally with a Seatbelt
   profile via `sandbox-exec` — so this slice covers **both** backends, not just
   Firecracker. Fail-closed on both. See [`../trust-model.md`](../trust-model.md)
   and [`../credential-proxy.md`](../credential-proxy.md) for the shipped
   mechanism and its accepted limitations (port-scoped not host-scoped; UDP
   unrestricted on Linux; host kernel ≥6.7).
4. **GitHub (separate, later).** No base-URL override exists for `gh`/`git`, and
   #73 already documented that URL/path filtering cannot constrain `gh api
   graphql`. Injection (token never crosses to the guest) is still worthwhile as
   an *exposure* reduction, but it is a different mechanism (credential helper or
   `url.insteadOf` rewriting) and must not be labeled "scoped." Treat it as its own
   design under #73's constraints.

Each slice is gated behind explicit config (proxy mode is opt-in), per coop's
"no speculative features / additive and opt-in" stance.

---

## 9. Threat model and residual risks

- **What it stops:** a prompt-injected/rogue agent reading a usable API key from
  its environment or from disk. After v1, in proxy mode, the two model API keys
  are never in the guest.
- **What it does not stop (by construction):**
  - *Use* of the injected credential while the agent runs — the agent can still
    make model calls through the proxy (that is the point). Non-exposure limits
    *exfiltration of the raw key*, not use during the session.
  - *Egress* — a token-less agent with open egress can still reach arbitrary hosts
    (#2). The proxy composes with #2 but does not replace it; the strong pairing is
    proxy + "no route out except the proxy."
  - *Scope* — the injected key retains its full account scope (#73). Non-exposure
    and scope are orthogonal.
- **New attack surface:** the proxy itself. Mitigated by (a) a mature HTTP stack,
  (b) fixed per-route upstreams (guest cannot retarget the injected key), (c)
  host-side jailing on Firecracker (§3, slice 3). This surface is the whole reason
  the helper VM stays on the table for the general proxy (§3/§4).
- **DNS/CDN caveats** from #2 are unaffected — this feature does not touch name
  resolution.

---

## 10. Rejected / deferred alternatives

- **Helper VM for the per-integration proxy.** Rejected for v1: the per-integration
  proxy's attack surface is small and CA-free, so a whole second VM (awkward on
  Lima, heavy against #404, backend-diverging) is disproportionate. Retained as the
  correct home for a *general* proxy (§3/§4).
- **General MITM proxy now.** Deferred: the guest-trusted CA and arbitrary-upstream
  TLS termination are a large build for coverage the per-integration path already
  gives on the high-value credentials (§4).
- **Keep forwarding raw keys, rely on the VM boundary alone.** Rejected: that is
  the status quo #411 exists to fix; with open egress (#2) a readable key is an
  exfiltratable key.
- **Reimplementing OAuth refresh in the proxy (esp. Codex subscription).**
  Rejected: the refresh loops run against internal, undocumented endpoints and
  rotate state a security product should not own against an unstable contract. Use
  the static-token paths (API key everywhere; Claude `setup-token` for Claude
  subscription) and route Codex subscription to API-key mode per OpenAI's own
  guidance (§7).

---

## 11. Open questions / spikes

1. ~~**Subscription endpoint semantics.**~~ **Resolved (§7).** Claude ships an
   official one-year `setup-token` (a static inference-scoped bearer) → proxy
   injects it, no refresh. Codex ships *no* long-lived token; its only subscription
   credential is a client-refreshed `auth.json`, and OpenAI recommends API keys for
   automation → Codex subscription is out of scope for the proxy, API-key mode is
   the answer. Net: only Codex-subscription would have needed refresh logic, and it
   is deliberately not built.
2. **One proxy process vs. per-instance.** Per-instance is simpler to reason about
   and matches the existing socket-lifecycle pattern; a shared process is fewer
   moving parts but needs per-instance routing. Decide in slice 1.
3. ~~**Firecracker jail specifics.**~~ **Resolved (shipped, slice 3).** Not the
   jailer/netns/uid options considered here — those fight the loopback + `ssh -R`
   architecture (an isolated netns cannot reach the host-side tunnel). The proxy
   self-confines with **Landlock** (ABI v4) on Linux and **Seatbelt** on macOS,
   with the lightest lifecycle (no sudo, no per-instance netns/route teardown).
   See [`../trust-model.md`](../trust-model.md).
4. **Config surface.** What the opt-in looks like (`[network]`/`[proxy]` block),
   and how it interacts with `coop model local` (proxy mode and local mode both
   rewrite `base_url` — they must not collide).

---

## 12. Packaging, install, and attestation

**The attestation is over the tarball, so bundle the proxy inside it.** The release
workflow attests each per-target artifact — `actions/attest-build-provenance` with
`subject-path: "coop-*.tar.gz"` (`release.yml`) — and `install.sh` verifies it with
`gh attestation verify <tarball> --repo trailofbits/coop`, falling back to the
published `SHA256SUMS`. Anything shipped *inside* that already-attested tarball
inherits the identical SLSA build-provenance guarantee with **no new attestation
machinery**. So `coop-proxy` ships in the same tarball as `coop`.

`coop-proxy` runs on the **host** (it holds the secret and forwards upstream), so
it builds for the exact three host triples already in the matrix
(`aarch64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`)
— no new build targets.

**Crate layout.** The root `Cargo.toml` today is a single `coop` package (the
`fuzz/` dir is its own separate workspace). Promote the root to a workspace that
keeps `coop` as the root package and adds a `coop-proxy/` member with its *own*
`Cargo.toml` carrying the async/HTTP/TLS deps (`hyper`, `rustls`, `tokio`). Because
`coop` does **not** depend on `coop-proxy`, the `coop` binary's dependency closure
is unchanged — the heavy, security-critical deps are isolated to and independently
auditable in the proxy crate (§6a). `cargo build --release --workspace` produces
both binaries under `target/<triple>/release/`.

**Release diff (small):**
- Build: `cargo build --release --workspace --target <triple>` (was: default
  package only).
- Package: `cp` **both** `coop` and `coop-proxy` into `staging/<name>/` before
  `tar`. Everything downstream — `SHA256SUMS`, the provenance attestation, the
  `gh release create` — is untouched, because it all operates on the tarball.

**Install diff (small):** after extraction, move **both** binaries into
`INSTALL_DIR`. Checksum + attestation verification are unchanged (still one tarball,
one subject). `coop` locates its proxy via `std::env::current_exe()`'s parent
directory, so they must land side by side — which the single-tarball install
guarantees.

**Why not a separate artifact per binary.** A standalone `coop-proxy-*.tar.gz` with
its own attestation doubles the release outputs, adds a second `gh attestation
verify` to install, and introduces version-skew risk (proxy and CLI from different
builds). Bundling gives **lockstep versions by construction** (both from one build)
and one thing to verify — strictly simpler at equal cryptographic strength.

**Why not a single binary with a hidden `coop proxy` subcommand (re-exec).** It is
the simplest packaging (nothing changes, one binary, trivially the same
attestation), and it keeps runtime isolation via re-exec + jail. But it links
`hyper`/`rustls`/`tokio` into the one `coop` binary, enlarging the main tool's
attack and supply-chain surface — the opposite of §6a's goal for a security-
critical tool. Rejected for that reason; the two-binaries-one-tarball approach
keeps the dependency surfaces separate at near-zero packaging cost.

---

## 13. Dependency and feature hygiene (enforced, not advised)

Adding an async/HTTP/TLS stack to a tool that currently depends only on `url` is
the largest new supply-chain surface in this design. The rule is **every new dep
enters with `default-features = false` and an explicit, minimal feature list**, and
that minimality is *enforced in CI* by the `cargo deny check` that already runs
(`ci.yml`, `cargo-deny@0.19.9`) — no new CI step, just new rules in `deny.toml`.
Because `coop-proxy` is a workspace member, its graph is already in scope of that
check.

**Enforcement: pin the exact feature set per direct dep.** cargo-deny's
`[[bans.features]]` with `exact = true` requires the *enabled* feature set to equal
the allowlist, so CI fails if anyone (or a transitive edge) turns on a feature we
did not sanction. Example (names/versions pinned against docs.rs at implementation
time):

```toml
[[bans.features]]
crate = "tokio"
exact = true
allow = ["rt-multi-thread", "net", "io-util", "macros", "signal"]

[[bans.features]]
crate = "hyper"
exact = true
allow = ["http1", "http2", "server", "client"]

[[bans.features]]
crate = "hyper-util"
exact = true
allow = ["tokio", "server", "client-legacy", "http1", "http2"]
```

with the matching `coop-proxy/Cargo.toml`:

```toml
tokio            = { version = "1",  default-features = false, features = ["rt-multi-thread", "net", "io-util", "macros", "signal"] }
hyper            = { version = "1",  default-features = false, features = ["http1", "http2", "server", "client"] }
hyper-util       = { version = "0.1", default-features = false, features = ["tokio", "server", "client-legacy", "http1", "http2"] }
http-body-util   = { version = "0.1", default-features = false }
tokio-rustls     = { version = "0.26", default-features = false, features = ["tls12"] }
rustls           = { version = "0.23", default-features = false, features = ["std", "tls12", "aws_lc_rs"] }
webpki-roots     = "1"   # pinned Mozilla roots (MPL-2.0, already allowed) — no OS trust-store dependency
```

`webpki-roots` (a compiled-in, pinned CA set) is chosen over `rustls-native-certs`
deliberately: two fixed upstreams need no OS trust-store variance, and a pinned
root set is both smaller surface and more deterministic for a security tool.

**Ban the escape hatches outright.** Keep the OpenSSL/system-TLS path out of the
graph so nothing silently pulls it in:

```toml
[bans]
deny = [{ crate = "openssl" }, { crate = "openssl-sys" }, { crate = "native-tls" }]
```

**The TLS crypto-provider license, scoped — not globally widened.** rustls itself
is `Apache-2.0`/`MIT`/`ISC`; its crypto backend is the snag. Both `aws-lc-rs`
(rustls default) and `ring` carry an **OpenSSL-family license** through their
C/`-sys` layer, and neither `OpenSSL` nor `ISC` is in coop's `[licenses] allow`
today. Handle this with a **crate-scoped exception**, so the global allowlist stays
tight:

```toml
[[licenses.exceptions]]
crate = "aws-lc-sys"          # scope OpenSSL/ISC to the crypto crate only
allow = ["OpenSSL", "ISC"]    # exact SPDX set verified against the pinned version
```

**Provider choice — a real tradeoff, flagged not hidden.** `aws-lc-rs` is the
recommended default (rustls default, AWS-maintained, FIPS-capable — the better
security posture) but builds C via `aws-lc-sys`, which needs cmake + a C
cross-toolchain for the two musl targets (the release runners already install
`musl-tools`; the aarch64 leg additionally needs the C cross-compiler already
present for the main build). `ring` cross-compiles more simply but the same
OpenSSL-license exception applies and its provider is less aligned with a
FIPS/security posture. Recommend `aws-lc-rs`; fall back to `ring` only if the
`aws-lc-sys` musl cross-build proves disproportionately costly. A pure-Rust
provider (e.g. `rustls-graviola`) is *not* recommended — an unproven crypto
implementation is a worse trade than a C build for a security-critical proxy.

**Complementary checks.** Run `cargo machete` (or `cargo-udeps`) to catch deps/
features that stop being used, and consider tightening `multiple-versions` from
`warn` to `deny` for the proxy graph so a duplicate transitive version is a
conscious decision. Neither is required for v1, but both keep the surface honest as
the crate evolves.

---

## 14. What remains for a highly solid feature (definition of done)

The design above is complete on paper; the following turns it into a feature you
would trust in a security product. Ordered by what gates the next thing.

### Tier 0 — prove the premise (do this first; it can invalidate the design)

Everything rests on one unproven assumption: **Claude Code and Codex both work,
end-to-end, pointed at a plain-HTTP header-injecting proxy — including SSE
streaming and tool-use round-trips — with no real credential in the guest.** Build
a throwaway hyper proxy in front of the *real* APIs and confirm:

- Claude Code with `ANTHROPIC_BASE_URL` = proxy and `ANTHROPIC_AUTH_TOKEN` = a
  placeholder: the proxy strips the guest header and injects the real credential as
  `x-api-key` (API key) **or** `Authorization: Bearer` (`setup-token`), plus
  `anthropic-version`. Confirm the API accepts it and Claude streams correctly.
- Codex with a custom provider `base_url` = proxy and no `env_key`: the proxy
  injects `Authorization: Bearer` to the Responses endpoint; streaming + tool use
  work.
- Neither agent pins certs, requires the token client-side for a feature, or sends
  an unstrippable identifying header that breaks injection.

If any of these fails, the mechanism changes before anything else is built.

### Tier 1 — security-critical correctness (the reason this feature exists)

- **Header-rewrite policy, audited and explicit.** Strip the guest capability
  token; inject exactly the required upstream auth header(s); pin the upstream
  `Host`; define an allowlist/strip rule for all other headers; reject
  smuggling/oversized/duplicate-auth attempts. This is the crown-jewel path — it
  gets a written spec and a security review.
- **Upstream is fixed, never guest-influenced.** The guest controls neither target
  host nor scheme; only the path is forwarded to the pinned host. Closes SSRF.
- **TLS verification is correct and cannot regress.** Verify the upstream cert
  against the pinned `webpki-roots`; a bug that disables verification MITMs the real
  key. Consider pinning the two upstreams. Add a test that a bad cert is rejected.
- **Fail closed.** If `cmd:` secret resolution fails at proxy start, the VM does
  not come up in a state where the agent silently has no or wrong credentials —
  clear error, no fallback to a raw-key path.
- **Capability token done right.** CSPRNG-minted, per-instance, constant-time
  compare, never logged; listener bound to the guest-only interface (never
  `0.0.0.0`); a test proves another instance's token and other host interfaces are
  rejected.
- **Resource limits.** Timeouts and concurrency/body-size caps so a rogue guest
  cannot exhaust host CPU/memory through the proxy.
- **Secret in memory only.** Resolved lazily, never written to disk or logs;
  redacted in all `Debug`; consider `zeroize` on drop.

### Tier 2 — robustness and operability

- **Lifecycle tied to the VM** (start/stop/destroy) via the `forwards.sock`
  supervision pattern; orphan/zombie cleanup if coop itself dies; port-collision
  pre-check.
- **Crash handling.** Proxy death mid-session surfaces a clear error (and/or
  restarts), not a silent hang.
- **Multi-instance isolation.** Per-instance proxy + per-instance token so
  instance A's guest can never reach instance B's credentials. (Resolves §11.2.)
- **`coop model local` interaction.** Proxy mode and local mode both rewrite
  `base_url`; define precedence/mutual-exclusion. (Resolves §11.4.)

### Tier 3 — validation gates (per CLAUDE.md)

- **The security property is asserted by a test, not just designed:** an
  integration phase, on **both** backends, that brings a VM up in proxy mode
  against a mock upstream and asserts (a) the request reaches the upstream carrying
  the injected header, (b) the raw credential appears **nowhere** in the guest env
  or disk, (c) `~/.codex/auth.json` is not staged, (d) streaming works, (e) a
  request without the capability token is refused.
- **Unit tests** for the pure logic (header rewrite, token check, route selection,
  proxy-mode guest-config generation, config precedence) — behavior, edges, error
  paths.
- **`.cargo/mutants.toml` scoping updated in the same PR** for the new pure
  helpers vs. the IO/backend/TTY functions (the doc is emphatic this is not a
  follow-up).
- **Security review** of the new surface (the `security-review` skill): SSRF,
  header/request smuggling, TLS-verify correctness, resource exhaustion, the
  host-reachable-port exposure on macOS.
- **Firecracker jailing implemented and validated** (slice 3), not just asserted.

### Tier 4 — UX, docs, and surface

- **Opt-in config shape** decided (default off, per coop's conservatism) and
  fuzzed like the other config parsers if it adds fields. (Resolves §11.4 config
  half.)
- **`coop status` shows proxy state** (on/off, which creds injected — redacted,
  health).
- **Docs page** stating the threat model honestly (§9): what non-exposure does and
  does not guarantee, and the weaker macOS story (host-local processes reach the
  port, guarded only by the capability token).
- **Renewal ergonomics** for the Claude `setup-token` (warn before the yearly
  expiry).

### Cross-platform hardening: Lima is weaker by default but closable

The blast-radius story is strongest on Firecracker (netns/uid jail, optional vsock,
guest-only bridge). On macOS/Lima the proxy defaults to a host-local TCP port
reachable by any local process, guarded only by the capability token. That gap is
**closable to near-Firecracker posture**, via macOS-native primitives — each with a
real caveat, and all VZ-driver-specific and needing a spike shared with #2's macOS
networking work:

- **Jail the proxy with Seatbelt (`sandbox-exec`).** A profile confining
  `coop-proxy` to: no filesystem writes, `connect` only to the two upstream
  hosts:443, `bind` only its one listener, no `exec`. The sandbox inherits to
  children and cannot be removed from inside — the macOS analog of the Linux
  netns/uid jail, and close to it in strength. **Caveat:** `sandbox-exec` is
  officially deprecated (still functional and widely used — Chromium, agent
  harnesses — and Apple has shipped no clean CLI successor), so it is a pragmatic
  but aging primitive.
- **Restrict egress, host-side.** Same two options as #2, and it must be host-side
  because the Lima guest also has passwordless sudo: (a) **no route out** —
  configure the guest with no default route, only a host-local link to the proxy
  (structural, airtight, also defeats DNS tunneling); or (b) a host **`pf`** anchor
  limiting the guest subnet to the proxy's listener (needs sudo, Lima-mode-
  dependent, carries the usual filter gaps). Under `vzNAT` the guest already has no
  VM-to-VM path, so this only has to constrain guest→internet.
- **Close the local-port exposure with vsock.** The VZ driver already uses vsock
  (Lima runs its own guest agent over it), so the host↔guest hop can move to vsock
  with a tiny guest-side vsock→`127.0.0.1` shim — leaving no host TCP port for other
  local processes to reach. A unix-domain socket (0600) forwarded in is the same
  idea. **Cost:** a small guest-side forwarder (the agents speak HTTP/TCP, so they
  cannot target vsock directly). If that cost is not paid, the per-instance
  capability token remains the guard.

Net: Seatbelt ≈ the Linux jail, no-route-out is identical strength on both, and
vsock closes the port gap — so Lima can get close. The honest residual differences:
Seatbelt is deprecated where Linux namespaces are first-class, the vsock transport
needs a guest shim (more native on Firecracker), and all of it is VZ-specific and
unproven until the spike. State this plainly rather than implying uniform strength.
