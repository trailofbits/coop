# Credential-injecting proxy (`[proxy]`)

Status: **v1 — Anthropic (Claude Code); Firecracker and Lima.** Opt-in; off by
default. Design: [`design/issue-411-injecting-proxy.md`](design/issue-411-injecting-proxy.md).

## What it does

Without proxy mode, coop forwards the raw `ANTHROPIC_API_KEY` into the guest as
an SSH `SendEnv` variable — a prompt-injected or rogue agent can read it from
its own environment and, with egress open, exfiltrate it.

With proxy mode, the raw credential **never enters the guest**. coop runs a
small host-side reverse proxy (`coop-proxy`) for the lifetime of the VM:

- The proxy binds host loopback and is exposed into the guest by a per-instance
  `ssh -R` reverse tunnel; the guest is pointed at `http://127.0.0.1:<port>`
  (via `ANTHROPIC_BASE_URL` in the managed `~/.claude/settings.json`) and holds
  only a **per-instance capability token** (as `ANTHROPIC_AUTH_TOKEN`).
- The proxy verifies that token (constant-time), strips it, injects the real
  credential (`x-api-key`, or `Authorization: Bearer` for a `setup-token`), and
  streams the request to the pinned upstream `api.anthropic.com` over TLS.
- `ANTHROPIC_API_KEY` is no longer forwarded into the guest.

The capability token is worthless off the host — it only authorizes the local
proxy, which holds the real key itself — so exfiltrating it gains a compromised
guest nothing.

## Enabling it

The easiest path is `coop proxy setup`: it takes a pasted Claude `setup-token`
(or `--api-key`), stores it in a secret backend of your choice (macOS Keychain /
Linux secret-service / 1Password / a 0600 file), and writes the `[proxy.anthropic]`
block for you with a `cmd:` reference — so the credential is never plaintext in
the config. To generate a subscription token first, run `claude setup-token` on
the host (needs a Claude subscription; the token is inference-scoped, ~1 year).

Or configure it by hand:

```toml
[proxy.anthropic]
credential = "cmd:op read op://Private/Anthropic/credential"  # or a plain key
auth = "api_key"   # api_key → x-api-key (default); bearer → a Claude setup-token
```

The `credential` uses the same `cmd:` resolution as every other coop secret and
is resolved on the **host** at VM start. For subscription billing without
exposure, run `claude setup-token` on the host, stash the printed one-year
token, reference it via `cmd:`, and set `auth = "bearer"`.

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
its guest). Not yet built: Codex (routes to API-key mode regardless), GitHub,
and the Firecracker uid/netns jail.
