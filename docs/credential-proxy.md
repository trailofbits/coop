# Credential-injecting proxy (`[proxy]`)

Status: **v1 — Anthropic (Claude Code) and OpenAI (Codex); Firecracker and
Lima.** Opt-in; off by default. Design:
[`design/issue-411-injecting-proxy.md`](design/issue-411-injecting-proxy.md).

## What it does

Without proxy mode, coop forwards the raw `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`
into the guest as SSH `SendEnv` variables — a prompt-injected or rogue agent can
read them from its own environment and, with egress open, exfiltrate them.

With proxy mode, the raw credential **never enters the guest**. coop runs a
small host-side reverse proxy (`coop-proxy`) — one process per (VM, provider) —
for the lifetime of the VM:

- The proxy binds host loopback and is exposed into the guest by a per-instance
  `ssh -R` reverse tunnel; the guest is pointed at `http://127.0.0.1:<port>` and
  holds only a **per-instance capability token**.
  - **Claude Code:** `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` in the
    managed `~/.claude/settings.json`.
  - **Codex:** a `[model_providers.coop_local]` block in `~/.codex/config.toml`
    (`base_url` at the proxy, `wire_api = "responses"`) with the capability
    token supplied as the provider's bearer `env_key`. Proxy mode pins no model
    — Codex keeps its own, only its egress is redirected.
- The proxy verifies that token (constant-time), strips it, injects the real
  credential (`x-api-key`, or `Authorization: Bearer`), and streams the request
  to the pinned upstream (`api.anthropic.com` / `api.openai.com`) over TLS.
- The raw `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` is no longer forwarded, and in
  proxy mode Codex's `~/.codex/auth.json` is **not** staged onto the guest disk
  (it holds a refreshable subscription token — the exact at-rest exposure #411
  closes). Codex subscription is therefore out of scope in proxy mode; use an
  OpenAI API key.

The capability token is worthless off the host — it only authorizes the local
proxy, which holds the real key itself — so exfiltrating it gains a compromised
guest nothing.

## Enabling it

The easiest path is `coop proxy setup`: it takes a pasted credential, stores it
in a secret backend of your choice (macOS Keychain / Linux secret-service /
1Password / a 0600 file), and writes the `[proxy.<provider>]` block for you with
a `cmd:` reference — so the credential is never plaintext in the config.

- **Anthropic (default):** `coop proxy setup` takes a Claude `setup-token`
  (subscription) or an API key with `--api-key`. To generate a token first, run
  `claude setup-token` on the host (needs a Claude subscription; the token is
  inference-scoped, ~1 year).
- **OpenAI (Codex):** `coop proxy setup --openai` takes an OpenAI API key
  (always injected as `Authorization: Bearer`).

Or configure it by hand:

```toml
[proxy.anthropic]
credential = "cmd:op read op://Private/Anthropic/credential"  # or a plain key
auth = "api_key"   # api_key → x-api-key (default); bearer → a Claude setup-token

[proxy.openai]
credential = "cmd:op read op://Private/OpenAI/credential"
auth = "bearer"    # OpenAI keys inject as Authorization: Bearer
```

The `credential` uses the same `cmd:` resolution as every other coop secret and
is resolved on the **host** at VM start. For Claude subscription billing without
exposure, run `claude setup-token` on the host, stash the printed one-year
token, reference it via `cmd:`, and set `auth = "bearer"`.

## Per-VM credential overrides

The `[proxy.<provider>]` blocks are the **defaults** for every VM. A single VM
can use a different credential — for per-project billing, scope, or revocation —
with `coop proxy setup --openai --vm <name>` (or `--anthropic --vm <name>`). The
override is stored in that instance's state (`<inst.dir>/proxy.json`), not in a
growing config table, and its secret is namespaced separately (`coop-openai-<vm>`).

Resolution per provider is **override → default → off**: the per-VM override
wins, else the config default, else the proxy is off for that provider. This is
purely host-side credential selection — the proxy binary and the per-VM
capability token are unchanged, so there is no new attack surface.

`coop proxy status` shows the defaults and every VM's overrides; `coop proxy
status --vm <name>` shows one VM's effective resolution. Credentials are shown
as their `cmd:` reference (a command, not the secret); a hand-written literal is
redacted.

Proxy mode applies only in **remote** model mode. `coop model <vm> local` takes
precedence (the VM routes at your local model server and the proxy is torn
down). If credential resolution fails at start, the VM **fails closed** — it
does not come up on a path where the agent silently has no or the wrong key.

## What it does and does not guarantee

It stops a rogue agent from **reading a usable key** from its environment or
disk. It does **not**:

- Stop **use** of the credential while the agent runs — the agent still makes
  model calls through the proxy (that is the point). Non-exposure limits
  exfiltration of the raw key, not use during the session.
- Provide **egress** control — a token-less agent with open egress can still
  reach arbitrary hosts (issue #2). The proxy composes with, but does not
  replace, "no route out except the proxy."
- Provide **scope** enforcement — the injected key keeps its full account scope
  (issue #73). Non-exposure and scope are orthogonal.

The proxy itself is new attack surface, mitigated by a fixed per-route upstream
(the guest controls only the path, never the host — closing SSRF), TLS
verification against a pinned root set, a required capability token, and
resource limits. The listener binds only host loopback and is reverse-tunnelled
to exactly one guest — never a non-loopback interface, never the LAN.

## Platform support

**Firecracker (Linux) and Lima (macOS).** The proxy binds `127.0.0.1` on the
host and is exposed into the guest with a per-instance `ssh -R` reverse tunnel,
so it works identically on both backends (each already keeps an SSH channel to
its guest). Not yet built: GitHub, and the Firecracker uid/netns jail.
